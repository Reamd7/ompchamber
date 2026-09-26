//! Tests for the small-model port, mirroring the JS suites
//! (`resolve.test.js`, `call.test.js`, `index.test.js`,
//! `runtime-providers.test.js`, `summarization.test.js`) over the module's
//! injectable seams.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tower::ServiceExt;

use crate::small_model::auth_store::MemoryAuthStore;
use crate::small_model::call::{CallDeps, CallParams, SmallModelError, call_small_model};
use crate::small_model::catalog::CatalogCache;
use crate::small_model::http::{FetchRequest, FetchResponse};
use crate::small_model::opencode_config::ConfigLayers;
use crate::small_model::resolve::{ResolveParams, resolve_small_model};
use crate::small_model::runtime_providers::RuntimeProviders;
use crate::small_model::service::{
    GenerateParams, OutputReserve, OverflowPolicy, SmallModelService,
};
use crate::small_model::summarization::{
    SummarizeParams, SummaryMode, sanitize_for_tts, summarize_text,
};

// ---------------------------------------------------------------------------
// Fakes
// ---------------------------------------------------------------------------

type Queued = Arc<Mutex<Vec<Result<FetchResponse, String>>>>;
type Captured = Arc<Mutex<Vec<FetchRequest>>>;

/// Front-first queued-response transport capturing every request.
fn queued_transport(
    responses: Vec<Result<FetchResponse, String>>,
) -> (crate::small_model::Fetch, Captured) {
    let queue: Queued = Arc::new(Mutex::new(responses));
    let requests: Captured = Arc::new(Mutex::new(Vec::new()));
    let fetch: crate::small_model::Fetch = {
        let queue = Arc::clone(&queue);
        let requests = Arc::clone(&requests);
        Arc::new(move |request: FetchRequest| {
            let queue = Arc::clone(&queue);
            let requests = Arc::clone(&requests);
            Box::pin(async move {
                requests
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(request);
                let mut queue = queue.lock().unwrap_or_else(|error| error.into_inner());
                if queue.is_empty() {
                    return Err("no queued response".to_string());
                }
                queue.remove(0)
            })
        })
    };
    (fetch, requests)
}

fn ok(content: &str) -> Result<FetchResponse, String> {
    Ok(FetchResponse {
        status: 200,
        headers: Vec::new(),
        body:
            json!({ "choices": [{ "message": { "content": content }, "finish_reason": "stop" }] })
                .to_string()
                .into_bytes(),
    })
}

fn anthropic_ok(text: &str) -> Result<FetchResponse, String> {
    json_response(json!({ "content": [{ "type": "text", "text": text }] }))
}

fn json_response(payload: Value) -> Result<FetchResponse, String> {
    Ok(FetchResponse {
        status: 200,
        headers: Vec::new(),
        body: payload.to_string().into_bytes(),
    })
}

fn config_reader_with(merged: Value) -> crate::small_model::ConfigReader {
    let layers = ConfigLayers {
        merged,
        ..Default::default()
    };
    Arc::new(move |_| layers.clone())
}

fn empty_config() -> crate::small_model::ConfigReader {
    Arc::new(|_| ConfigLayers::default())
}

fn last_request(requests: &Captured) -> FetchRequest {
    requests
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .last()
        .cloned()
        .expect("a request was made")
}

fn header<'a>(request: &'a FetchRequest, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn body_json(request: &FetchRequest) -> Value {
    serde_json::from_slice(request.body.as_deref().unwrap_or(&[])).unwrap_or(Value::Null)
}

fn temp_dir_for(label: &str) -> std::path::PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "sm-{label}-{}-{}-{unique}",
        std::process::id(),
        crate::small_model::http::now_ms()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A dead-network transport that still records requests (for paths that must
/// not reach the wire).
fn no_network_transport() -> (crate::small_model::Fetch, Captured) {
    let requests: Captured = Arc::new(Mutex::new(Vec::new()));
    let fetch: crate::small_model::Fetch = {
        let requests = Arc::clone(&requests);
        Arc::new(move |request: FetchRequest| {
            let requests = Arc::clone(&requests);
            Box::pin(async move {
                requests
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(request);
                Err("no network in this test".to_string())
            })
        })
    };
    (fetch, requests)
}

// ---------------------------------------------------------------------------
// resolve.test.js — resolution precedence ladder
// ---------------------------------------------------------------------------

mod resolution {
    use super::*;
    use crate::small_model::resolve::{is_usable_auth_entry, parse_model_ref};

    fn catalog() -> Value {
        json!({
            "google": {
                "id": "google",
                "models": {
                    "gemini-2.5-flash": { "id": "gemini-2.5-flash", "family": "gemini-flash", "release_date": "2025-06-01" },
                    "gemini-2.0-flash": { "id": "gemini-2.0-flash", "family": "gemini-flash", "release_date": "2024-12-01" },
                    "gemini-2.5-pro": { "id": "gemini-2.5-pro", "family": "gemini-pro", "release_date": "2025-06-01" }
                }
            },
            "anthropic": {
                "id": "anthropic",
                "models": {
                    "claude-haiku-4-5": { "id": "claude-haiku-4-5", "family": "claude-haiku", "release_date": "2025-10-01" },
                    "claude-sonnet-4-5": { "id": "claude-sonnet-4-5", "family": "claude-sonnet", "release_date": "2025-09-01" }
                }
            }
        })
    }

    #[test]
    fn parse_model_ref_splits_on_the_first_slash() {
        assert_eq!(
            parse_model_ref("anthropic/claude-haiku-4-5"),
            Some(("anthropic".to_string(), "claude-haiku-4-5".to_string()))
        );
        assert_eq!(
            parse_model_ref("openrouter/google/gemini-2.5-flash"),
            Some((
                "openrouter".to_string(),
                "google/gemini-2.5-flash".to_string()
            ))
        );
        assert_eq!(parse_model_ref("anthropic/"), None);
        assert_eq!(parse_model_ref("/model"), None);
        assert_eq!(parse_model_ref("plain"), None);
    }

    #[test]
    fn usable_auth_entries() {
        assert!(is_usable_auth_entry(
            &json!({ "type": "api", "key": "sk-x" })
        ));
        assert!(is_usable_auth_entry(
            &json!({ "type": "oauth", "access": "a", "refresh": "r" })
        ));
        assert!(is_usable_auth_entry(
            &json!({ "type": "wellknown", "key": "k", "token": "t" })
        ));
        assert!(!is_usable_auth_entry(&json!({ "type": "api", "key": "" })));
        assert!(!is_usable_auth_entry(&json!({ "type": "oauth" })));
        assert!(!is_usable_auth_entry(&Value::Null));
    }

    fn resolve(
        auth: Value,
        settings: Option<&str>,
        config: Option<&str>,
        preferred_provider: Option<&str>,
        preferred_model: Option<&str>,
    ) -> Option<crate::small_model::resolve::ResolvedModel> {
        resolve_small_model(ResolveParams {
            auth: &auth,
            catalog: &catalog(),
            settings_small_model: settings,
            config_small_model: config,
            preferred_provider_id: preferred_provider,
            preferred_model_id: preferred_model,
        })
    }

    #[test]
    fn settings_override_outranks_everything() {
        let found = resolve(
            json!({ "anthropic": { "type": "api", "key": "sk-x" } }),
            Some("anthropic/claude-haiku-4-5"),
            Some("openai/gpt-4o-mini"),
            Some("anthropic"),
            None,
        )
        .unwrap();
        assert_eq!(
            (
                found.provider_id.as_str(),
                found.model_id.as_str(),
                found.source
            ),
            ("anthropic", "claude-haiku-4-5", "settings")
        );
    }

    #[test]
    fn configured_small_model_wins_over_scans() {
        let found = resolve(
            json!({ "anthropic": { "type": "api", "key": "sk-x" } }),
            None,
            Some("openai/gpt-4o-mini"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            (found.provider_id.as_str(), found.source),
            ("openai", "config")
        );
    }

    #[test]
    fn family_scan_prefers_gemini_flash_and_newest_release() {
        let found = resolve(
            json!({
                "google": { "type": "api", "key": "g" },
                "anthropic": { "type": "api", "key": "sk-x" }
            }),
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            (
                found.provider_id.as_str(),
                found.model_id.as_str(),
                found.source
            ),
            ("google", "gemini-2.5-flash", "family-scan")
        );
    }

    #[test]
    fn skips_providers_without_a_usable_credential() {
        let found = resolve(
            json!({
                "google": { "type": "api", "key": "" },
                "anthropic": { "type": "api", "key": "sk-x" }
            }),
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            (found.provider_id.as_str(), found.model_id.as_str()),
            ("anthropic", "claude-haiku-4-5")
        );
    }

    #[test]
    fn copilot_utility_fallback_when_only_copilot_is_logged_in() {
        let found = resolve(
            json!({ "github-copilot": { "type": "oauth", "access": "t", "refresh": "t" } }),
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(found.provider_id, "github-copilot");
        assert_eq!(found.source, "copilot-utility");
    }

    #[test]
    fn returns_none_when_nothing_is_authenticated() {
        assert!(resolve(json!({}), None, None, None, None).is_none());
    }

    #[test]
    fn session_provider_wins_over_other_authenticated_providers() {
        let found = resolve(
            json!({
                "google": { "type": "api", "key": "g" },
                "anthropic": { "type": "api", "key": "sk-x" }
            }),
            None,
            None,
            Some("anthropic"),
            None,
        )
        .unwrap();
        assert_eq!(
            (found.provider_id.as_str(), found.model_id.as_str()),
            ("anthropic", "claude-haiku-4-5")
        );
    }

    #[test]
    fn ignores_a_preferred_provider_without_a_usable_login() {
        let found = resolve(
            json!({ "google": { "type": "api", "key": "g" } }),
            None,
            None,
            Some("anthropic"),
            None,
        )
        .unwrap();
        assert_eq!(found.provider_id, "google");
    }

    #[test]
    fn never_uses_an_unauthenticated_session_provider_opencode_free_models() {
        // Vanilla setups default the picker to opencode/big-pickle with no
        // opencode token — those free models only work through OpenCode.
        let found = resolve(
            json!({ "openai": { "type": "oauth", "access": "a", "refresh": "r" } }),
            None,
            None,
            Some("opencode"),
            Some("big-pickle"),
        )
        .unwrap();
        assert_eq!(
            (
                found.provider_id.as_str(),
                found.model_id.as_str(),
                found.source
            ),
            ("openai", "gpt-5.4-mini", "codex-small")
        );
    }

    #[test]
    fn falls_back_to_the_session_model_instead_of_scanning_other_providers() {
        let found = resolve(
            json!({
                "opencode-go": { "type": "api", "key": "oc" },
                "openai": { "type": "oauth", "access": "a", "refresh": "r" }
            }),
            None,
            None,
            Some("opencode-go"),
            Some("deepseek-v4-flash"),
        )
        .unwrap();
        assert_eq!(
            (
                found.provider_id.as_str(),
                found.model_id.as_str(),
                found.source
            ),
            ("opencode-go", "deepseek-v4-flash", "session-model")
        );
    }

    #[test]
    fn falls_back_to_the_session_model_when_nothing_resolves() {
        let found = resolve(
            json!({ "mistral": { "type": "api", "key": "m" } }),
            None,
            None,
            Some("mistral"),
            Some("mistral-large-latest"),
        )
        .unwrap();
        assert_eq!(
            (
                found.provider_id.as_str(),
                found.model_id.as_str(),
                found.source
            ),
            ("mistral", "mistral-large-latest", "session-model")
        );
    }
}

// ---------------------------------------------------------------------------
// call.test.js — payload/error mapping on the fake transport
// ---------------------------------------------------------------------------

mod wire {
    use super::*;

    async fn call(
        auth: Value,
        catalog: Value,
        config: crate::small_model::ConfigReader,
        responses: Vec<Result<FetchResponse, String>>,
        provider_id: &str,
        model_id: &str,
    ) -> Result<(String, Captured), SmallModelError> {
        let (transport, requests) = queued_transport(responses);
        let deps = CallDeps::new(
            transport,
            config,
            Arc::new(MemoryAuthStore::new(auth)),
            RuntimeProviders::unwired(),
        );
        let auth_snapshot = deps.auth_store.read().unwrap_or_else(|_| Value::Null);
        let text = call_small_model(
            &deps,
            CallParams {
                auth: &auth_snapshot,
                catalog: &catalog,
                working_directory: Some("/proj"),
                provider_id,
                model_id,
                prompt: "hi",
                system: None,
                max_output_tokens: None,
                response_schema: None,
                timeout_ms: None,
            },
        )
        .await?;
        Ok((text, requests))
    }

    #[tokio::test]
    async fn uses_config_api_key_and_base_url() {
        let config = config_reader_with(json!({
            "provider": { "custom": { "options": { "apiKey": "test-key", "baseURL": "https://proxy.example.test/v1" } } }
        }));
        let (text, requests) = call(
            json!({}),
            json!({}),
            config,
            vec![ok("hello")],
            "custom",
            "gpt-4o-mini",
        )
        .await
        .unwrap();
        assert_eq!(text, "hello");
        let request = last_request(&requests);
        assert_eq!(
            request.url,
            "https://proxy.example.test/v1/chat/completions"
        );
        assert!(!request.url.contains("api.openai.com"));
        assert_eq!(header(&request, "Authorization"), Some("Bearer test-key"));
    }

    #[tokio::test]
    async fn resolves_env_api_key_variables() {
        // SAFETY: unique variable name; no other test touches it.
        unsafe {
            std::env::set_var("OMPCHAMBER_TEST_PROVIDER_KEY", "sk-env-key");
        }
        let config = config_reader_with(json!({
            "provider": { "custom": { "options": { "apiKey": "{env:OMPCHAMBER_TEST_PROVIDER_KEY}", "baseURL": "https://proxy.example.test/v1" } } }
        }));
        let result = call(
            json!({}),
            json!({}),
            config,
            vec![ok("hello")],
            "custom",
            "model",
        )
        .await;
        unsafe {
            std::env::remove_var("OMPCHAMBER_TEST_PROVIDER_KEY");
        }
        let (_, requests) = result.unwrap();
        assert_eq!(
            header(&last_request(&requests), "Authorization"),
            Some("Bearer sk-env-key")
        );
    }

    #[tokio::test]
    async fn sends_configured_headers_alongside_the_bearer() {
        // SAFETY: unique variable name; no other test touches it.
        unsafe {
            std::env::set_var("OMPCHAMBER_TEST_GATEWAY_KEY", "sub-key");
        }
        let config = config_reader_with(json!({
            "provider": { "custom": { "options": {
                "apiKey": "sk-config",
                "baseURL": "https://proxy.example.test/v1",
                "headers": {
                    "Ocp-Apim-Subscription-Key": "{env:OMPCHAMBER_TEST_GATEWAY_KEY}",
                    "x-tenant": "team"
                }
            } } }
        }));
        let result = call(
            json!({}),
            json!({}),
            config,
            vec![ok("hello")],
            "custom",
            "model",
        )
        .await;
        unsafe {
            std::env::remove_var("OMPCHAMBER_TEST_GATEWAY_KEY");
        }
        let (_, requests) = result.unwrap();
        let request = last_request(&requests);
        assert_eq!(
            header(&request, "Ocp-Apim-Subscription-Key"),
            Some("sub-key")
        );
        assert_eq!(header(&request, "x-tenant"), Some("team"));
        assert_eq!(header(&request, "Authorization"), Some("Bearer sk-config"));
    }

    #[tokio::test]
    async fn configured_headers_override_authorization_case_insensitively() {
        let config = config_reader_with(json!({
            "provider": { "custom": { "options": {
                "apiKey": "sk-config",
                "baseURL": "https://proxy.example.test/v1",
                "headers": { "authorization": "Basic gateway-token" }
            } } }
        }));
        let (_, requests) = call(
            json!({}),
            json!({}),
            config,
            vec![ok("hello")],
            "custom",
            "model",
        )
        .await
        .unwrap();
        let request = last_request(&requests);
        assert_eq!(
            header(&request, "authorization"),
            Some("Basic gateway-token")
        );
        assert_eq!(
            request
                .headers
                .iter()
                .filter(|(key, _)| key.eq_ignore_ascii_case("authorization"))
                .count(),
            1,
            "single Authorization header after the case-insensitive override"
        );
    }

    #[tokio::test]
    async fn trims_a_trailing_slash_from_the_configured_base_url() {
        let config = config_reader_with(json!({
            "provider": { "custom": { "options": { "apiKey": "k", "baseURL": "https://proxy.example.test/v1/" } } }
        }));
        let (_, requests) = call(json!({}), json!({}), config, vec![ok("ok")], "custom", "m")
            .await
            .unwrap();
        assert_eq!(
            last_request(&requests).url,
            "https://proxy.example.test/v1/chat/completions"
        );
    }

    #[tokio::test]
    async fn throws_no_login_before_any_network_call() {
        let config = config_reader_with(json!({
            "provider": { "custom": { "options": { "baseURL": "https://proxy.example.test/v1" } } }
        }));
        let error = call(
            json!({}),
            json!({}),
            config,
            vec![],
            "custom",
            "gpt-4o-mini",
        )
        .await
        .unwrap_err();
        assert_eq!(error.status_code, 401);
        assert_eq!(error.code.as_deref(), Some("no-provider-login"));
        assert_eq!(
            error.message,
            "No OpenCode login found for provider \"custom\""
        );
        assert_eq!(error.provider_id.as_deref(), Some("custom"));
    }

    #[tokio::test]
    async fn treats_a_blank_config_api_key_as_absent() {
        let config = config_reader_with(json!({
            "provider": { "custom": { "options": { "apiKey": "   ", "baseURL": "https://x.test/v1" } } }
        }));
        let error = call(
            json!({}),
            json!({}),
            config,
            vec![],
            "custom",
            "gpt-4o-mini",
        )
        .await
        .unwrap_err();
        assert_eq!(error.code.as_deref(), Some("no-provider-login"));
    }

    #[tokio::test]
    async fn uses_auth_json_credential_with_config_base_url() {
        let config = config_reader_with(json!({
            "provider": { "custom": { "options": { "baseURL": "https://proxy.example.test/v1" } } }
        }));
        let (_, requests) = call(
            json!({ "custom": { "type": "api", "key": "authjson-key" } }),
            json!({}),
            config,
            vec![ok("done")],
            "custom",
            "gpt-4o-mini",
        )
        .await
        .unwrap();
        let request = last_request(&requests);
        assert_eq!(
            request.url,
            "https://proxy.example.test/v1/chat/completions"
        );
        assert_eq!(
            header(&request, "Authorization"),
            Some("Bearer authjson-key")
        );
    }

    #[tokio::test]
    async fn config_api_key_wins_over_auth_json() {
        let config = config_reader_with(json!({
            "provider": { "custom": { "options": { "apiKey": "config-key", "baseURL": "https://proxy.example.test/v1" } } }
        }));
        let (_, requests) = call(
            json!({ "custom": { "type": "api", "key": "authjson-key" } }),
            json!({}),
            config,
            vec![ok("done")],
            "custom",
            "gpt-4o-mini",
        )
        .await
        .unwrap();
        let request = last_request(&requests);
        assert_eq!(header(&request, "Authorization"), Some("Bearer config-key"));
        assert!(
            !request
                .body
                .as_deref()
                .and_then(|body| std::str::from_utf8(body).ok())
                .is_some_and(|body| body.contains("authjson-key"))
        );
    }

    #[tokio::test]
    async fn openai_base_url_override_and_default() {
        let config = config_reader_with(json!({
            "provider": { "openai": { "options": { "baseURL": "https://gateway.example.test/v1" } } }
        }));
        let (_, requests) = call(
            json!({ "openai": { "type": "api", "key": "sk-openai" } }),
            json!({}),
            config,
            vec![ok("ok")],
            "openai",
            "gpt-4o-mini",
        )
        .await
        .unwrap();
        assert_eq!(
            last_request(&requests).url,
            "https://gateway.example.test/v1/chat/completions"
        );

        let (_, requests) = call(
            json!({ "openai": { "type": "api", "key": "sk-openai" } }),
            json!({}),
            empty_config(),
            vec![ok("ok")],
            "openai",
            "gpt-4o-mini",
        )
        .await
        .unwrap();
        assert_eq!(
            last_request(&requests).url,
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[tokio::test]
    async fn a_base_url_alone_does_not_authenticate_openai() {
        let config = config_reader_with(json!({
            "provider": { "openai": { "options": { "baseURL": "https://gateway.example.test/v1" } } }
        }));
        let error = call(
            json!({}),
            json!({}),
            config,
            vec![],
            "openai",
            "gpt-4o-mini",
        )
        .await
        .unwrap_err();
        assert_eq!(error.code.as_deref(), Some("no-provider-login"));
    }

    #[tokio::test]
    async fn anthropic_base_url_shapes() {
        let config = config_reader_with(json!({
            "provider": { "anthropic": { "options": { "baseURL": "http://127.0.0.1:3456/v1" } } }
        }));
        let (_, requests) = call(
            json!({ "anthropic": { "type": "api", "key": "dummy" } }),
            json!({}),
            config,
            vec![anthropic_ok("ok")],
            "anthropic",
            "claude-haiku-4-5",
        )
        .await
        .unwrap();
        let request = last_request(&requests);
        assert_eq!(request.url, "http://127.0.0.1:3456/v1/messages");
        assert_eq!(header(&request, "x-api-key"), Some("dummy"));

        let config = config_reader_with(json!({
            "provider": { "anthropic": { "options": { "baseURL": "http://127.0.0.1:3456" } } }
        }));
        let (_, requests) = call(
            json!({ "anthropic": { "type": "api", "key": "dummy" } }),
            json!({}),
            config,
            vec![anthropic_ok("ok")],
            "anthropic",
            "claude-haiku-4-5",
        )
        .await
        .unwrap();
        assert_eq!(
            last_request(&requests).url,
            "http://127.0.0.1:3456/messages"
        );

        let (_, requests) = call(
            json!({ "anthropic": { "type": "api", "key": "sk-ant" } }),
            json!({}),
            empty_config(),
            vec![anthropic_ok("ok")],
            "anthropic",
            "claude-haiku-4-5",
        )
        .await
        .unwrap();
        assert_eq!(
            last_request(&requests).url,
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[tokio::test]
    async fn catalog_api_url_used_without_config_override() {
        let catalog = json!({
            "mistral": {
                "id": "mistral",
                "name": "Mistral",
                "api": "https://api.mistral.ai/v1",
                "models": { "mistral-small-latest": { "id": "mistral-small-latest" } }
            }
        });
        let (_, requests) = call(
            json!({ "mistral": { "type": "api", "key": "mistral-key" } }),
            catalog,
            empty_config(),
            vec![ok("ok")],
            "mistral",
            "mistral-small-latest",
        )
        .await
        .unwrap();
        let request = last_request(&requests);
        assert_eq!(request.url, "https://api.mistral.ai/v1/chat/completions");
        assert_eq!(
            header(&request, "Authorization"),
            Some("Bearer mistral-key")
        );
    }

    #[tokio::test]
    async fn errors_when_no_base_url_is_known() {
        let error = call(
            json!({ "custom": { "type": "api", "key": "k" } }),
            json!({}),
            empty_config(),
            vec![],
            "custom",
            "gpt-4o-mini",
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.message,
            "Provider \"custom\" has no known API base URL"
        );
    }

    fn runtime_with_provider(
        provider: crate::small_model::runtime_providers::RuntimeProvider,
    ) -> Arc<RuntimeProviders> {
        let payload = json!({
            "all": [{
                "id": provider.id,
                "source": provider.source,
                "options": {
                    "apiKey": provider.api_key.clone().unwrap_or_default(),
                    "baseURL": provider.base_url.clone().unwrap_or_default()
                },
                "models": {}
            }],
            "connected": [provider.id]
        });
        RuntimeProviders::new(Arc::new(move || {
            let payload = payload.clone();
            Box::pin(async move { Ok(Some(payload)) })
        }))
    }

    async fn call_with_runtime(
        runtime: Arc<RuntimeProviders>,
        auth: Value,
        catalog: Value,
        config: crate::small_model::ConfigReader,
        responses: Vec<Result<FetchResponse, String>>,
        provider_id: &str,
        model_id: &str,
    ) -> Result<Captured, SmallModelError> {
        let (transport, requests) = queued_transport(responses);
        let auth_store: Arc<dyn crate::small_model::AuthStore> =
            Arc::new(MemoryAuthStore::new(auth));
        let auth_snapshot = auth_store.read().unwrap_or_else(|_| Value::Null);
        let deps = CallDeps::new(transport, config, auth_store, runtime);
        call_small_model(
            &deps,
            CallParams {
                auth: &auth_snapshot,
                catalog: &catalog,
                working_directory: Some("/proj"),
                provider_id,
                model_id,
                prompt: "hi",
                system: None,
                max_output_tokens: None,
                response_schema: None,
                timeout_ms: None,
            },
        )
        .await?;
        Ok(requests)
    }

    #[tokio::test]
    async fn uses_the_runtime_endpoint_and_credential_for_a_plugin_provider() {
        let runtime =
            runtime_with_provider(crate::small_model::runtime_providers::RuntimeProvider {
                id: "llmapi".to_string(),
                source: Some("config".to_string()),
                api_key: Some("plugin-key".to_string()),
                base_url: Some("https://api.llmapi.ai/v1".to_string()),
                anonymous_zen: false,
            });
        let requests = call_with_runtime(
            runtime,
            json!({}),
            json!({}),
            empty_config(),
            vec![ok("done")],
            "llmapi",
            "claude-opus-4-8",
        )
        .await
        .unwrap();
        let request = last_request(&requests);
        assert_eq!(request.url, "https://api.llmapi.ai/v1/chat/completions");
        assert_eq!(header(&request, "Authorization"), Some("Bearer plugin-key"));
    }

    #[tokio::test]
    async fn keeps_chatgpt_plan_login_off_the_runtime_key() {
        // OpenCode reports an OAuth access token as options.apiKey for
        // openai; api.openai.com answers it with 401, so the runtime
        // credential must not stand in for the codex path.
        let runtime =
            runtime_with_provider(crate::small_model::runtime_providers::RuntimeProvider {
                id: "openai".to_string(),
                source: None,
                api_key: Some("oauth-access-token".to_string()),
                base_url: None,
                anonymous_zen: false,
            });
        let error = call_with_runtime(
            runtime,
            json!({}),
            json!({}),
            empty_config(),
            vec![],
            "openai",
            "gpt-5.4-mini",
        )
        .await
        .unwrap_err();
        assert_eq!(error.code.as_deref(), Some("no-provider-login"));
    }

    #[tokio::test]
    async fn config_base_url_beats_the_runtime_endpoint() {
        let runtime =
            runtime_with_provider(crate::small_model::runtime_providers::RuntimeProvider {
                id: "custom".to_string(),
                source: None,
                api_key: Some("runtime-key".to_string()),
                base_url: Some("https://runtime.example/v1".to_string()),
                anonymous_zen: false,
            });
        let config = config_reader_with(json!({
            "provider": { "custom": { "options": { "baseURL": "https://configured.example/v1" } } }
        }));
        let requests = call_with_runtime(
            runtime,
            json!({ "custom": { "type": "api", "key": "auth-key" } }),
            json!({}),
            config,
            vec![ok("done")],
            "custom",
            "m",
        )
        .await
        .unwrap();
        assert_eq!(
            last_request(&requests).url,
            "https://configured.example/v1/chat/completions"
        );
    }

    // --- Google thinking configuration ------------------------------------

    fn google_response(text: &str) -> Result<FetchResponse, String> {
        json_response(json!({ "candidates": [{ "content": { "parts": [{ "text": text }] } }] }))
    }

    async fn google_body(model_id: &str) -> Value {
        let (_, requests) = call(
            json!({ "google": { "type": "api", "key": "google-key" } }),
            json!({}),
            empty_config(),
            vec![google_response("generated")],
            "google",
            model_id,
        )
        .await
        .unwrap();
        body_json(&last_request(&requests))
    }

    #[tokio::test]
    async fn google_gemini3_flash_uses_thinking_level_minimal() {
        let body = google_body("gemini-3.1-flash-lite-preview").await;
        assert_eq!(
            body["generationConfig"]["thinkingConfig"],
            json!({ "thinkingLevel": "minimal" })
        );
    }

    #[tokio::test]
    async fn google_gemini2_disables_the_thinking_budget() {
        let body = google_body("gemini-2.5-flash-lite").await;
        assert_eq!(
            body["generationConfig"]["thinkingConfig"],
            json!({ "thinkingBudget": 0 })
        );
    }

    #[tokio::test]
    async fn google_other_models_omit_thinking_config() {
        let body = google_body("gemini-1.5-flash").await;
        assert!(body["generationConfig"].get("thinkingConfig").is_none());
    }

    // --- GitHub Copilot endpoint routing ----------------------------------

    fn copilot_auth(enterprise: Option<&str>) -> Value {
        let mut entry = json!({
            "type": "oauth",
            "access": "test-token",
            "refresh": "test-token",
            "expires": 0
        });
        if let Some(url) = enterprise {
            entry["enterpriseUrl"] = json!(url);
        }
        json!({ "github-copilot": entry })
    }

    async fn call_copilot(
        responses: Vec<Result<FetchResponse, String>>,
        model_id: &str,
        enterprise: Option<&str>,
    ) -> (Result<String, SmallModelError>, Captured) {
        let (transport, requests) = queued_transport(responses);
        let auth_store: Arc<dyn crate::small_model::AuthStore> =
            Arc::new(MemoryAuthStore::new(copilot_auth(enterprise)));
        let auth_snapshot = auth_store.read().unwrap_or_else(|_| Value::Null);
        let deps = CallDeps::new(
            transport,
            empty_config(),
            auth_store,
            RuntimeProviders::unwired(),
        );
        let result = call_small_model(
            &deps,
            CallParams {
                auth: &auth_snapshot,
                catalog: &json!({}),
                working_directory: Some("/proj"),
                provider_id: "github-copilot",
                model_id,
                prompt: "summarize this diff",
                system: Some("Write a commit message"),
                max_output_tokens: Some(100),
                response_schema: None,
                timeout_ms: None,
            },
        )
        .await;
        (result, requests)
    }

    fn captured(requests: &Captured) -> Vec<FetchRequest> {
        requests
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    #[tokio::test]
    async fn copilot_routes_responses_endpoint_models() {
        let (result, requests) = call_copilot(
            vec![
                json_response(json!({ "data": [{ "id": "mai-code-1-flash-picker", "supported_endpoints": ["/responses"] }] })),
                json_response(json!({ "output": [{ "type": "message", "content": [{ "type": "output_text", "text": "feat: add summary" }] }] })),
            ],
            "mai-code-1-flash-picker",
            None,
        )
        .await;
        assert_eq!(result.unwrap(), "feat: add summary");
        let all = captured(&requests);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].url, "https://api.githubcopilot.com/models");
        assert_eq!(all[1].url, "https://api.githubcopilot.com/responses");
        let body = body_json(&all[1]);
        assert_eq!(body["model"], json!("mai-code-1-flash-picker"));
        assert_eq!(body["instructions"], json!("Write a commit message"));
        assert_eq!(body["max_output_tokens"], json!(100));
        assert_eq!(body["stream"], json!(false));
        assert_eq!(body["store"], json!(false));
        assert!(
            !all[1]
                .body
                .as_deref()
                .and_then(|body| std::str::from_utf8(body).ok())
                .is_some_and(|body| body.contains("test-token"))
        );
    }

    #[tokio::test]
    async fn copilot_prefers_messages_endpoint() {
        let (result, requests) = call_copilot(
            vec![
                json_response(json!({ "data": [{ "id": "claude-opus-4.7", "supported_endpoints": ["/chat/completions", "/responses", "/v1/messages"] }] })),
                json_response(json!({ "content": [{ "type": "text", "text": "fix: route Claude correctly" }] })),
            ],
            "claude-opus-4.7",
            None,
        )
        .await;
        assert_eq!(result.unwrap(), "fix: route Claude correctly");
        let all = captured(&requests);
        assert_eq!(all[1].url, "https://api.githubcopilot.com/v1/messages");
        let body = body_json(&all[1]);
        assert_eq!(body["model"], json!("claude-opus-4.7"));
        assert_eq!(body["system"], json!("Write a commit message"));
        assert_eq!(body["max_tokens"], json!(100));
    }

    #[tokio::test]
    async fn copilot_chat_endpoint_and_legacy_default() {
        let (result, requests) = call_copilot(
            vec![
                json_response(json!({ "data": [{ "id": "gpt-5.4-nano", "supported_endpoints": ["/chat/completions"] }] })),
                ok("chore: update summary"),
            ],
            "gpt-5.4-nano",
            None,
        )
        .await;
        assert_eq!(result.unwrap(), "chore: update summary");
        assert_eq!(
            captured(&requests)[1].url,
            "https://api.githubcopilot.com/chat/completions"
        );

        let (result, requests) = call_copilot(
            vec![
                json_response(json!({ "data": [{ "id": "gpt-4o-mini" }] })),
                ok("docs: clarify behavior"),
            ],
            "gpt-4o-mini",
            None,
        )
        .await;
        assert_eq!(result.unwrap(), "docs: clarify behavior");
        assert_eq!(
            captured(&requests)[1].url,
            "https://api.githubcopilot.com/chat/completions"
        );
    }

    #[tokio::test]
    async fn copilot_enterprise_host() {
        let (result, requests) = call_copilot(
            vec![
                json_response(
                    json!({ "data": [{ "id": "m", "supported_endpoints": ["/responses"] }] }),
                ),
                json_response(json!({ "output_text": "fix: support enterprise routing" })),
            ],
            "m",
            Some("https://ghe.example.com/"),
        )
        .await;
        assert_eq!(result.unwrap(), "fix: support enterprise routing");
        let all = captured(&requests);
        assert_eq!(all[0].url, "https://copilot-api.ghe.example.com/models");
        assert_eq!(all[1].url, "https://copilot-api.ghe.example.com/responses");
    }

    #[tokio::test]
    async fn copilot_metadata_failures_surface_without_generating() {
        let (result, requests) = call_copilot(
            vec![Ok(FetchResponse {
                status: 503,
                headers: Vec::new(),
                body: b"{\"error\":\"unavailable\"}".to_vec(),
            })],
            "mai-code-1-flash-picker",
            None,
        )
        .await;
        assert_eq!(
            result.unwrap_err().message,
            "GitHub Copilot models request failed with 503: {\"error\":\"unavailable\"}"
        );
        assert_eq!(captured(&requests).len(), 1);

        let (result, _) = call_copilot(
            vec![json_response(json!({ "data": [{ "id": "different-model", "supported_endpoints": ["/responses"] }] }))],
            "mai-code-1-flash-picker",
            None,
        )
        .await;
        assert_eq!(
            result.unwrap_err().message,
            "GitHub Copilot model \"mai-code-1-flash-picker\" was not returned by /models"
        );

        let (result, _) = call_copilot(
            vec![json_response(json!({ "data": [{ "id": "mai-code-1-flash-picker", "supported_endpoints": ["/future"] }] }))],
            "mai-code-1-flash-picker",
            None,
        )
        .await;
        assert_eq!(
            result.unwrap_err().message,
            "GitHub Copilot model \"mai-code-1-flash-picker\" has no supported text endpoint"
        );
    }

    // --- Structured output -------------------------------------------------

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": { "title": { "type": "string" } },
            "required": ["title"],
            "additionalProperties": false,
        })
    }

    async fn call_with_schema(
        provider_id: &str,
        model_id: &str,
        auth: Value,
        schema: Option<Value>,
        response: Result<FetchResponse, String>,
    ) -> (Result<String, SmallModelError>, Captured) {
        let (transport, requests) = queued_transport(vec![response]);
        let auth_store: Arc<dyn crate::small_model::AuthStore> =
            Arc::new(MemoryAuthStore::new(auth));
        let auth_snapshot = auth_store.read().unwrap_or_else(|_| Value::Null);
        let deps = CallDeps::new(
            transport,
            empty_config(),
            auth_store,
            RuntimeProviders::unwired(),
        );
        let result = call_small_model(
            &deps,
            CallParams {
                auth: &auth_snapshot,
                catalog: &json!({}),
                working_directory: Some("/proj"),
                provider_id,
                model_id,
                prompt: "summarize",
                system: None,
                max_output_tokens: None,
                response_schema: schema.as_ref(),
                timeout_ms: None,
            },
        )
        .await;
        (result, requests)
    }

    #[tokio::test]
    async fn sends_json_schema_response_format_on_openai_chat() {
        let (result, requests) = call_with_schema(
            "openai",
            "gpt-5.4-mini",
            json!({ "openai": { "type": "api", "key": "sk-test" } }),
            Some(schema()),
            ok("{\"title\":\"ok\"}"),
        )
        .await;
        assert_eq!(result.unwrap(), "{\"title\":\"ok\"}");
        let body = body_json(&last_request(&requests));
        assert_eq!(
            body["response_format"],
            json!({
                "type": "json_schema",
                "json_schema": { "name": "response", "strict": true, "schema": schema() }
            })
        );
    }

    #[tokio::test]
    async fn omits_response_format_without_a_schema() {
        let (.., requests) = call_with_schema(
            "openai",
            "gpt-5.4-mini",
            json!({ "openai": { "type": "api", "key": "sk-test" } }),
            None,
            ok("plain text"),
        )
        .await;
        let body = body_json(&last_request(&requests));
        assert!(body.get("response_format").is_none());
    }

    #[tokio::test]
    async fn anthropic_structured_output_forces_a_tool_call() {
        let (result, requests) = call_with_schema(
            "anthropic",
            "claude-haiku-4-5",
            json!({ "anthropic": { "type": "api", "key": "sk-ant" } }),
            Some(schema()),
            json_response(json!({
                "content": [
                    { "type": "text", "text": "thinking out loud" },
                    { "type": "tool_use", "name": "response", "input": { "title": "ok" } }
                ]
            })),
        )
        .await;
        assert_eq!(result.unwrap(), "{\"title\":\"ok\"}");
        let body = body_json(&last_request(&requests));
        assert_eq!(
            body["tool_choice"],
            json!({ "type": "tool", "name": "response" })
        );
        assert_eq!(body["tools"][0]["input_schema"], schema());
    }

    #[tokio::test]
    async fn anthropic_prose_instead_of_tool_call_fails_loudly() {
        let (result, _) = call_with_schema(
            "anthropic",
            "claude-haiku-4-5",
            json!({ "anthropic": { "type": "api", "key": "sk-ant" } }),
            Some(schema()),
            json_response(json!({ "content": [{ "type": "text", "text": "here you go" }] })),
        )
        .await;
        assert!(
            result
                .unwrap_err()
                .message
                .contains("returned no structured output")
        );
    }

    #[tokio::test]
    async fn google_strips_unsupported_schema_keywords() {
        let mut schema = schema();
        schema["$schema"] = json!("https://json-schema.org/draft/2020-12/schema");
        let (.., requests) = call_with_schema(
            "google",
            "gemini-2.5-flash",
            json!({ "google": { "type": "api", "key": "google-key" } }),
            Some(schema),
            json_response(json!({ "candidates": [{ "content": { "parts": [{ "text": "{\"title\":\"ok\"}" }] } }] })),
        )
        .await;
        let body = body_json(&last_request(&requests));
        assert_eq!(
            body["generationConfig"]["responseMimeType"],
            json!("application/json")
        );
        assert_eq!(
            body["generationConfig"]["responseSchema"],
            json!({
                "type": "object",
                "properties": { "title": { "type": "string" } },
                "required": ["title"]
            })
        );
    }

    #[tokio::test]
    async fn chatgpt_plan_refuses_a_schema_without_a_request() {
        let (result, requests) = call_with_schema(
            "openai",
            "gpt-5.4-mini",
            json!({ "openai": { "type": "oauth", "access": "token", "refresh": "refresh", "expires": 0 } }),
            Some(schema()),
            ok("never reached"),
        )
        .await;
        let error = result.unwrap_err();
        assert_eq!(error.code.as_deref(), Some("structured-output-unsupported"));
        assert!(error.message.contains("does not support structured output"));
        assert!(captured(&requests).is_empty());
    }

    #[tokio::test]
    async fn output_exhausted_when_only_reasoning_was_produced() {
        let response = FetchResponse {
            status: 200,
            headers: Vec::new(),
            body: json!({
                "choices": [{
                    "message": { "content": "", "reasoning_content": "thinking..." },
                    "finish_reason": "length"
                }]
            })
            .to_string()
            .into_bytes(),
        };
        let (result, _) = call_with_schema(
            "openai",
            "gpt-5.4-mini",
            json!({ "openai": { "type": "api", "key": "sk" } }),
            None,
            Ok(response),
        )
        .await;
        let error = result.unwrap_err();
        assert_eq!(error.code.as_deref(), Some("output-exhausted"));
        assert!(
            error
                .message
                .contains("spent the output budget on reasoning")
        );
        assert!(error.message.contains("(finish_reason: length)"));
    }

    fn base64_encode_url_safe_no_pad(text: &str) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(text)
    }

    #[tokio::test]
    async fn openai_oauth_refreshes_then_streams_codex_responses() {
        // Expired token → refresh → codex SSE. The JWT carries
        // chatgpt_account_id "acct-123" in its claims.
        let claims = base64_encode_url_safe_no_pad(
            &json!({ "https://api.openai.com/auth": { "chatgpt_account_id": "acct-123" } })
                .to_string(),
        );
        let access_token = format!("header.{claims}.signature");
        let refresh_response = FetchResponse {
            status: 200,
            headers: Vec::new(),
            body: json!({
                "access_token": access_token,
                "refresh_token": "rotated-refresh",
                "expires_in": 3600
            })
            .to_string()
            .into_bytes(),
        };
        let sse = format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({ "type": "response.output_text.delta", "delta": "sum" }),
            json!({ "type": "response.output_text.done", "text": "summary done" })
        );
        let codex_response = FetchResponse {
            status: 200,
            headers: Vec::new(),
            body: sse.into_bytes(),
        };
        let (transport, requests) =
            queued_transport(vec![Ok(refresh_response), Ok(codex_response)]);
        let auth_store: Arc<dyn crate::small_model::AuthStore> = Arc::new(MemoryAuthStore::new(
            json!({ "openai": { "type": "oauth", "access": "", "refresh": "r-token", "expires": 0 } }),
        ));
        let auth_snapshot = auth_store.read().unwrap_or_else(|_| Value::Null);
        let deps = CallDeps::new(
            transport,
            empty_config(),
            auth_store,
            RuntimeProviders::unwired(),
        );
        let text = call_small_model(
            &deps,
            CallParams {
                auth: &auth_snapshot,
                catalog: &json!({}),
                working_directory: Some("/proj"),
                provider_id: "openai",
                model_id: "gpt-5.4-mini",
                prompt: "summarize",
                system: None,
                max_output_tokens: None,
                response_schema: None,
                timeout_ms: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(text, "summary done");
        let all = captured(&requests);
        assert_eq!(all.len(), 2, "refresh then codex call");
        assert_eq!(all[0].url, "https://auth.openai.com/oauth/token");
        assert_eq!(
            all[1].url,
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            header(&all[1], "Authorization"),
            Some(format!("Bearer {access_token}").as_str())
        );
        assert_eq!(header(&all[1], "ChatGPT-Account-Id"), Some("acct-123"));
        // The rotated refresh token is written back to the auth store.
        let written = deps.auth_store.read().unwrap();
        assert_eq!(written["openai"]["refresh"], json!("rotated-refresh"));
        assert_eq!(written["openai"]["access"], json!(access_token));
    }
}

// ---------------------------------------------------------------------------
// index.test.js — service orchestration, budgets, routes
// ---------------------------------------------------------------------------

mod service_suite {
    use super::*;

    fn budget_catalog() -> Value {
        json!({
            "anthropic": {
                "id": "anthropic",
                "models": {
                    "claude-haiku-4-5": { "id": "claude-haiku-4-5", "limit": { "context": 8_000 }, "structured_output": true },
                    "legacy-tiny": { "id": "legacy-tiny", "limit": { "context": 8_000 }, "structured_output": false },
                    "unlisted-capability": { "id": "unlisted-capability", "limit": { "context": 8_000 } },
                    "roomy": { "id": "roomy", "limit": { "context": 100_000, "output": 8_000 } },
                    "unlisted": { "id": "unlisted", "limit": { "context": 100_000 } }
                }
            }
        })
    }

    fn catalog_fetch_for(catalog: Value) -> crate::small_model::Fetch {
        let catalog = Arc::new(catalog);
        Arc::new(move |_request| {
            let body = catalog.to_string();
            Box::pin(async move {
                Ok(FetchResponse {
                    status: 200,
                    headers: Vec::new(),
                    body: body.into_bytes(),
                })
            })
        })
    }

    fn service_with(
        auth: Value,
        catalog: Value,
        settings: Option<Value>,
        responses: Vec<Result<FetchResponse, String>>,
    ) -> (Arc<SmallModelService>, Captured) {
        let (transport, requests) = queued_transport(responses);
        let dir = temp_dir_for("service");
        if let Some(settings) = settings {
            std::fs::write(dir.join("settings.json"), settings.to_string()).unwrap();
        }
        let deps = CallDeps::new(
            transport,
            empty_config(),
            Arc::new(MemoryAuthStore::new(auth)),
            RuntimeProviders::unwired(),
        );
        let service = SmallModelService::new(
            deps,
            CatalogCache::new(catalog_fetch_for(catalog), dir.join("catalog.json")),
            dir.join("settings.json"),
        );
        (service, requests)
    }

    fn simple_generate(model: &str, prompt: &str) -> GenerateParams {
        GenerateParams {
            prompt: Some(prompt.to_string()),
            model: Some(model.to_string()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn rejects_claude_code_before_transport_dispatch() {
        let (service, requests) = service_with(
            json!({ "claude-code": { "type": "oauth", "access": "cli", "refresh": "cli" } }),
            json!({}),
            None,
            vec![ok("unused")],
        );
        let error = service
            .generate(simple_generate("claude-code/haiku", "summarize this"))
            .await
            .unwrap_err();
        assert_eq!(error.status_code, 422);
        assert_eq!(
            error.code.as_deref(),
            Some("small-model-provider-unsupported")
        );
        assert!(captured_requests(&requests).is_empty());
    }

    fn captured_requests(requests: &Captured) -> Vec<FetchRequest> {
        requests
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    #[tokio::test]
    async fn does_not_offer_claude_code_in_the_picker() {
        let (service, _) = service_with(
            json!({ "claude-code": { "type": "oauth", "access": "cli", "refresh": "cli" } }),
            json!({}),
            None,
            vec![ok("unused")],
        );
        assert!(
            !service
                .list_authenticated_providers()
                .await
                .contains(&"claude-code".to_string())
        );
    }

    #[tokio::test]
    async fn runtime_plugin_providers_join_the_picker_but_not_endpointless_or_zen() {
        let payload = json!({
            "all": [
                { "id": "llmapi", "options": { "apiKey": "plugin-key", "baseURL": "https://api.llmapi.ai/v1" }, "models": {} },
                { "id": "endpointless", "options": { "apiKey": "plugin-key" }, "models": {} },
                { "id": "opencode", "options": { "apiKey": "public" }, "models": { "free": { "api": { "url": "https://opencode.ai/zen/v1" } } } }
            ],
            "connected": ["llmapi", "endpointless", "opencode"]
        });
        let runtime = RuntimeProviders::new(Arc::new(move || {
            let payload = payload.clone();
            Box::pin(async move { Ok(Some(payload)) })
        }));
        let (transport, _) = no_network_transport();
        let dir = temp_dir_for("providers");
        let deps = CallDeps::new(
            transport,
            empty_config(),
            Arc::new(MemoryAuthStore::new(json!({
                "openai": { "type": "api", "key": "sk-test" }
            }))),
            runtime,
        );
        let service = SmallModelService::new(
            deps,
            CatalogCache::new(catalog_fetch_for(json!({})), dir.join("catalog.json")),
            dir.join("settings.json"),
        );
        let providers = service.list_authenticated_providers().await;
        assert!(providers.contains(&"openai".to_string()));
        assert!(providers.contains(&"llmapi".to_string()));
        assert!(!providers.contains(&"endpointless".to_string()));
        assert!(!providers.contains(&"opencode".to_string()));
    }

    #[tokio::test]
    async fn oversized_input_truncates_by_default_and_flags_the_response() {
        let (service, requests) = service_with(
            json!({ "anthropic": { "type": "api", "key": "sk-ant" } }),
            budget_catalog(),
            None,
            vec![anthropic_ok("generated")],
        );
        let result = service
            .generate(simple_generate(
                "anthropic/claude-haiku-4-5",
                &"x".repeat(20_000),
            ))
            .await
            .unwrap();
        assert_eq!(result.input_truncated, Some(true));
        let sent = body_json(&last_request(&requests))["messages"][0]["content"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(sent.chars().count() < 20_000);
        assert!(sent.ends_with('\u{2026}'));
    }

    #[tokio::test]
    async fn oversized_input_refuses_under_the_error_policy() {
        let (service, requests) = service_with(
            json!({ "anthropic": { "type": "api", "key": "sk-ant" } }),
            budget_catalog(),
            None,
            vec![anthropic_ok("generated")],
        );
        let error = service
            .generate(GenerateParams {
                on_overflow: OverflowPolicy::Error,
                ..simple_generate("anthropic/claude-haiku-4-5", &"x".repeat(20_000))
            })
            .await
            .unwrap_err();
        assert_eq!(error.status_code, 413);
        assert_eq!(error.code.as_deref(), Some("context-too-small"));
        assert_eq!(error.required_chars, Some(20_000));
        assert_eq!(error.available_chars, Some(16_000));
        assert!(captured_requests(&requests).is_empty());
    }

    #[tokio::test]
    async fn fitting_input_passes_through_untouched() {
        let (service, requests) = service_with(
            json!({ "anthropic": { "type": "api", "key": "sk-ant" } }),
            budget_catalog(),
            None,
            vec![anthropic_ok("generated")],
        );
        let result = service
            .generate(GenerateParams {
                on_overflow: OverflowPolicy::Error,
                ..simple_generate("anthropic/claude-haiku-4-5", "short prompt")
            })
            .await
            .unwrap();
        assert_eq!(result.input_truncated, None);
        assert_eq!(
            body_json(&last_request(&requests))["messages"][0]["content"],
            json!("short prompt")
        );
    }

    #[tokio::test]
    async fn settings_override_outranks_the_resolution_chain() {
        let (service, _) = service_with(
            json!({ "anthropic": { "type": "api", "key": "sk-ant" } }),
            budget_catalog(),
            Some(json!({
                "smallModelUseDefault": false,
                "smallModelOverride": "anthropic/roomy"
            })),
            vec![anthropic_ok("generated")],
        );
        let result = service
            .generate(GenerateParams {
                prompt: Some("short".to_string()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(result.source, "settings");
        assert_eq!(result.model_id, "roomy");
    }

    #[tokio::test]
    async fn restrict_to_preferred_provider_blocks_silent_switching() {
        // The preferred provider has no login, so resolution falls through to
        // google's family scan — which the restriction must refuse.
        let (service, _) = service_with(
            json!({ "google": { "type": "api", "key": "g" } }),
            json!({
                "google": { "models": { "gemini-2.5-flash": { "id": "gemini-2.5-flash", "family": "gemini-flash", "release_date": "2025-06-01" } } }
            }),
            None,
            // The unrestricted follow-up call goes to google's wire format.
            vec![json_response(json!({
                "candidates": [{ "content": { "parts": [{ "text": "generated" }] } }]
            }))],
        );
        let error = service
            .generate(GenerateParams {
                prompt: Some("hi".to_string()),
                preferred_provider_id: Some("anthropic".to_string()),
                restrict_to_preferred_provider: true,
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_eq!(
            error.status_code, 404,
            "unexpected error: {}",
            error.message
        );
        assert_eq!(
            error.message,
            "No small model available within the session provider"
        );

        // Without the restriction the same request switches providers freely.
        let result = service
            .generate(GenerateParams {
                prompt: Some("hi".to_string()),
                preferred_provider_id: Some("anthropic".to_string()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(result.provider_id, "google");
        assert_eq!(result.source, "family-scan");
    }

    #[tokio::test]
    async fn output_budget_is_capped_by_the_catalog_limit() {
        let (service, requests) = service_with(
            json!({ "anthropic": { "type": "api", "key": "sk-ant" } }),
            budget_catalog(),
            None,
            vec![anthropic_ok("generated"), anthropic_ok("generated")],
        );
        service
            .generate(GenerateParams {
                max_output_tokens: Some(24_000),
                ..simple_generate("anthropic/roomy", "short")
            })
            .await
            .unwrap();
        assert_eq!(
            body_json(&last_request(&requests))["max_tokens"],
            json!(8_000)
        );

        service
            .generate(GenerateParams {
                max_output_tokens: Some(24_000),
                ..simple_generate("anthropic/unlisted", "short")
            })
            .await
            .unwrap();
        assert_eq!(
            body_json(&last_request(&requests))["max_tokens"],
            json!(24_000)
        );
    }

    #[tokio::test]
    async fn input_reserve_matches_the_requested_output_budget() {
        let (service, requests) = service_with(
            json!({ "anthropic": { "type": "api", "key": "sk-ant" } }),
            budget_catalog(),
            None,
            vec![anthropic_ok("generated")],
        );
        // 100k context − 24k reserve = 76k tokens = 304k chars.
        let error = service
            .generate(GenerateParams {
                on_overflow: OverflowPolicy::Error,
                max_output_tokens: Some(24_000),
                ..simple_generate("anthropic/unlisted", &"x".repeat(304_001))
            })
            .await
            .unwrap_err();
        assert_eq!(error.available_chars, Some(304_000));

        service
            .generate(GenerateParams {
                on_overflow: OverflowPolicy::Error,
                max_output_tokens: Some(24_000),
                ..simple_generate("anthropic/unlisted", &"x".repeat(303_999))
            })
            .await
            .unwrap();
        assert_eq!(captured_requests(&requests).len(), 1);
    }

    #[tokio::test]
    async fn describe_reports_capabilities_and_budget() {
        let (service, _) = service_with(
            json!({ "anthropic": { "type": "api", "key": "sk-ant" } }),
            budget_catalog(),
            None,
            vec![anthropic_ok("generated")],
        );
        let described = service
            .describe_small_model(
                Some("/proj"),
                None,
                None,
                OutputReserve::Tokens(None),
                Some("anthropic/claude-haiku-4-5"),
            )
            .await
            .unwrap()
            .expect("resolves");
        assert!(described.has_login);
        assert_eq!(described.input_char_budget, 16_000);
        assert_eq!(described.context_tokens, 8_000);
        assert!(described.context_known);
        assert_eq!(described.structured_output, Some(true));

        let described = service
            .describe_small_model(
                Some("/proj"),
                None,
                None,
                OutputReserve::Tokens(None),
                Some("anthropic/legacy-tiny"),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(described.structured_output, Some(false));

        let described = service
            .describe_small_model(
                Some("/proj"),
                None,
                None,
                OutputReserve::Tokens(None),
                Some("anthropic/unlisted-capability"),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(described.structured_output, None);

        // No login: an explicit override can name a provider without one.
        let (empty_service, _) = service_with(
            json!({}),
            budget_catalog(),
            None,
            vec![anthropic_ok("generated")],
        );
        let described = empty_service
            .describe_small_model(
                Some("/proj"),
                None,
                None,
                OutputReserve::Tokens(None),
                Some("anthropic/claude-haiku-4-5"),
            )
            .await
            .unwrap()
            .unwrap();
        assert!(!described.has_login);
    }

    #[tokio::test]
    async fn describe_reserve_function_sees_the_resolved_limits() {
        let (service, _) = service_with(
            json!({ "anthropic": { "type": "api", "key": "sk-ant" } }),
            budget_catalog(),
            None,
            vec![anthropic_ok("generated")],
        );
        let described = service
            .describe_small_model(
                Some("/proj"),
                None,
                None,
                OutputReserve::FromLimits(Arc::new(|limits| {
                    limits
                        .output_token_limit
                        .map(|limit| (limits.context_tokens / 10).min(limit))
                })),
                Some("anthropic/roomy"),
            )
            .await
            .unwrap()
            .unwrap();
        // 100k context, 8k output limit -> 8k reserved, leaving 92k tokens.
        assert_eq!(described.output_tokens, Some(8_000));
        assert_eq!(described.input_char_budget, 92_000 * 4);

        let described = service
            .describe_small_model(
                Some("/proj"),
                None,
                None,
                OutputReserve::Tokens(Some(24_000)),
                Some("anthropic/unlisted"),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(described.output_tokens, Some(24_000));
        assert_eq!(described.input_char_budget, 304_000);
    }

    // --- Routes -----------------------------------------------------------

    async fn call_route(
        service: Arc<SmallModelService>,
        method: &str,
        uri: &str,
        body: Option<(Value, &str)>,
    ) -> axum::response::Response {
        let router = crate::small_model::routes::routes(service);
        let mut request = axum::http::Request::builder().method(method).uri(uri);
        let body_bytes = match body {
            Some((payload, content_type)) => {
                request = request.header("content-type", content_type);
                axum::body::Body::from(payload.to_string())
            }
            None => axum::body::Body::empty(),
        };
        router
            .oneshot(request.body(body_bytes).unwrap())
            .await
            .unwrap()
    }

    async fn response_json(response: axum::response::Response) -> (axum::http::StatusCode, Value) {
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }

    #[tokio::test]
    async fn get_small_model_reports_resolution_preview() {
        let (service, _) = service_with(
            json!({ "anthropic": { "type": "api", "key": "sk-ant" } }),
            budget_catalog(),
            Some(json!({
                "smallModelUseDefault": false,
                "smallModelOverride": "anthropic/claude-haiku-4-5"
            })),
            vec![anthropic_ok("generated")],
        );
        let (status, payload) =
            response_json(call_route(service, "GET", "/api/small-model", None).await).await;
        assert_eq!(status, 200);
        assert_eq!(payload["available"], json!(true));
        assert!(
            payload["authenticatedProviders"]
                .as_array()
                .unwrap()
                .contains(&json!("anthropic"))
        );
        assert_eq!(payload["model"]["providerID"], json!("anthropic"));
    }

    #[tokio::test]
    async fn post_generate_returns_the_generation_shape() {
        let (service, _) = service_with(
            json!({ "anthropic": { "type": "api", "key": "sk-ant" } }),
            budget_catalog(),
            None,
            vec![anthropic_ok("  generated  ")],
        );
        let (status, payload) = response_json(
            call_route(
                service,
                "POST",
                "/api/small-model/generate",
                Some((
                    json!({ "prompt": "short", "model": "anthropic/roomy" }),
                    "application/json",
                )),
            )
            .await,
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(payload["text"], json!("generated"));
        assert_eq!(payload["providerID"], json!("anthropic"));
        assert_eq!(payload["modelID"], json!("roomy"));
        assert_eq!(payload["source"], json!("request"));
        assert!(payload.get("inputTruncated").is_none());
    }

    #[tokio::test]
    async fn post_generate_maps_error_status_codes_and_messages() {
        // 404: no small model → message from the error.
        let (service, _) = service_with(json!({}), budget_catalog(), None, vec![ok("unused")]);
        let (status, payload) = response_json(
            call_route(
                service,
                "POST",
                "/api/small-model/generate",
                Some((json!({ "prompt": "hi" }), "application/json")),
            )
            .await,
        )
        .await;
        assert_eq!(status, 404);
        assert_eq!(
            payload["error"],
            json!("No small model available — no authenticated provider has a suitable model")
        );

        // 400: missing prompt → fixed UI message.
        let (service, _) = service_with(
            json!({ "anthropic": { "type": "api", "key": "sk-ant" } }),
            budget_catalog(),
            None,
            vec![anthropic_ok("unused")],
        );
        let (status, payload) = response_json(
            call_route(
                service,
                "POST",
                "/api/small-model/generate",
                Some((json!({}), "application/json")),
            )
            .await,
        )
        .await;
        assert_eq!(status, 400);
        assert_eq!(
            payload["error"],
            json!(
                "The selected Small Model could not complete this action. Choose another model in Settings → Sessions → Small Model and try again."
            )
        );

        // 422: claude-code → fixed UI message + code.
        let (service, _) = service_with(
            json!({ "claude-code": { "type": "oauth", "access": "c", "refresh": "c" } }),
            json!({}),
            None,
            vec![ok("unused")],
        );
        let (status, payload) = response_json(
            call_route(
                service,
                "POST",
                "/api/small-model/generate",
                Some((
                    json!({ "prompt": "hi", "model": "claude-code/haiku" }),
                    "application/json",
                )),
            )
            .await,
        )
        .await;
        assert_eq!(status, 422);
        assert_eq!(payload["code"], json!("small-model-provider-unsupported"));
        assert!(
            payload["error"]
                .as_str()
                .unwrap()
                .starts_with("The selected Small Model")
        );
    }

    #[tokio::test]
    async fn post_generate_requires_a_json_content_type_like_express() {
        let (service, _) = service_with(
            json!({ "anthropic": { "type": "api", "key": "sk-ant" } }),
            budget_catalog(),
            None,
            vec![anthropic_ok("unused")],
        );
        let (status, _) = response_json(
            call_route(
                service,
                "POST",
                "/api/small-model/generate",
                Some((json!({ "prompt": "hi" }), "text/plain")),
            )
            .await,
        )
        .await;
        assert_eq!(status, 400);
    }
}

// ---------------------------------------------------------------------------
// Audit seam contract (session_goal wiring)
// ---------------------------------------------------------------------------

mod audit_seam {
    use super::*;
    use crate::session_goal::{AuditError, AuditRequest};

    fn audit_service_with(
        auth: Value,
        response: Result<FetchResponse, String>,
    ) -> crate::session_goal::AuditService {
        // Resolution must land on the anthropic family model for the wire
        // call to happen at all.
        let catalog = json!({
            "anthropic": { "models": { "claude-haiku-4-5": { "id": "claude-haiku-4-5", "family": "claude-haiku" } } }
        });
        let (transport, _) = queued_transport(vec![response]);
        let dir = temp_dir_for("audit");
        let deps = CallDeps::new(
            transport,
            empty_config(),
            Arc::new(MemoryAuthStore::new(auth)),
            RuntimeProviders::unwired(),
        );
        let catalog_fetch: crate::small_model::Fetch = Arc::new(move |_request| {
            let body = catalog.to_string();
            Box::pin(async move {
                Ok(FetchResponse {
                    status: 200,
                    headers: Vec::new(),
                    body: body.into_bytes(),
                })
            })
        });
        let service = SmallModelService::new(
            deps,
            CatalogCache::new(catalog_fetch, dir.join("catalog.json")),
            dir.join("settings.json"),
        );
        crate::small_model::audit_service(service)
    }

    fn audit_request() -> AuditRequest {
        // The goal loop always forwards the session's provider/model hints;
        // without them restrictToPreferredProvider would rightly refuse.
        AuditRequest {
            prompt: "audit prompt".to_string(),
            system: "system".to_string(),
            directory: "/proj".to_string(),
            preferred_provider_id: Some("anthropic".to_string()),
            preferred_model_id: Some("claude-haiku-4-5".to_string()),
        }
    }

    #[tokio::test]
    async fn ok_generation_maps_to_audit_output() {
        let audit = audit_service_with(
            json!({ "anthropic": { "type": "api", "key": "sk" } }),
            anthropic_ok("  verdict text  "),
        );
        match audit(audit_request()).await {
            Ok(output) => {
                assert_eq!(output.text, "verdict text");
                assert_eq!(output.provider_id.as_deref(), Some("anthropic"));
                assert!(output.model_id.is_some());
            }
            Err(AuditError::Failed(message)) => panic!("expected Ok, got Failed: {message}"),
            Err(AuditError::Unavailable) => panic!("expected Ok, got Unavailable"),
        }
    }

    #[tokio::test]
    async fn no_small_model_maps_to_unavailable() {
        let audit = audit_service_with(json!({}), anthropic_ok("never"));
        match audit(audit_request()).await {
            Err(AuditError::Unavailable) => {}
            Err(AuditError::Failed(message)) => {
                panic!("expected Unavailable, got Failed: {message}")
            }
            Ok(_) => panic!("expected Unavailable, got Ok"),
        }
    }

    #[tokio::test]
    async fn transport_failure_maps_to_failed_with_the_message() {
        let audit = audit_service_with(
            json!({ "anthropic": { "type": "api", "key": "sk" } }),
            Ok(FetchResponse {
                status: 503,
                headers: Vec::new(),
                body: Vec::new(),
            }),
        );
        match audit(audit_request()).await {
            Err(AuditError::Failed(message)) => {
                assert!(message.contains("Anthropic request failed with 503"))
            }
            Err(AuditError::Unavailable) => panic!("expected Failed, got Unavailable"),
            Ok(_) => panic!("expected Failed, got Ok"),
        }
    }
}

// ---------------------------------------------------------------------------
// summarization.test.js — budgeting and fallbacks
// ---------------------------------------------------------------------------

mod summarization_suite {
    use super::*;

    #[test]
    fn tts_removes_code_before_punctuation() {
        assert_eq!(
            sanitize_for_tts("Read `const value = 1` aloud"),
            "Read aloud"
        );
        assert_eq!(
            sanitize_for_tts("Before\n```js\nconst value = 1\n```\nAfter"),
            "Before After"
        );
    }

    #[tokio::test]
    async fn notification_mode_clips_to_max_length_with_ellipsis() {
        let result = summarize_text(
            SummarizeParams {
                text: Some("The implementation now correctly loads notification templates before dispatching the notification. It also fetches the latest assistant message when the event payload does not include message parts. This should make completion notifications match user settings."),
                threshold: 0,
                max_length: Some(80.0),
                mode: SummaryMode::Notification,
            },
            None,
        )
        .await;
        assert_eq!(result["summarized"], json!(false));
        assert_eq!(
            result["reason"],
            json!("Model summarization provider unavailable")
        );
        assert_eq!(
            result["summary"],
            json!(
                "The implementation now correctly loads notification templates before dispatchin\u{2026}"
            )
        );
    }

    #[tokio::test]
    async fn note_mode_returns_the_first_sentence() {
        let result = summarize_text(
            SummarizeParams {
                text: Some("First sentence. Second sentence with the useful insight."),
                threshold: 0,
                max_length: Some(100.0),
                mode: SummaryMode::Note,
            },
            None,
        )
        .await;
        assert_eq!(result["summary"], json!("First sentence."));
        assert_eq!(result["summarized"], json!(false));
        assert_eq!(
            result["reason"],
            json!("Model summarization provider unavailable")
        );
    }

    #[tokio::test]
    async fn budgeting_threshold_decides_the_reason() {
        let under = summarize_text(
            SummarizeParams {
                text: Some("short"),
                threshold: 200,
                max_length: Some(500.0),
                mode: SummaryMode::Tts,
            },
            None,
        )
        .await;
        assert_eq!(under["reason"], json!("Text under threshold"));
        assert!(under.get("originalLength").is_none());

        let empty = summarize_text(
            SummarizeParams {
                text: None,
                threshold: 200,
                max_length: Some(500.0),
                mode: SummaryMode::Tts,
            },
            None,
        )
        .await;
        assert_eq!(empty["reason"], json!("No text provided"));
        assert_eq!(empty["summary"], json!(""));

        let over = summarize_text(
            SummarizeParams {
                text: Some(&"word ".repeat(100)),
                threshold: 200,
                max_length: Some(500.0),
                mode: SummaryMode::Tts,
            },
            None,
        )
        .await;
        assert_eq!(
            over["reason"],
            json!("Model summarization provider unavailable")
        );
        assert_eq!(over["originalLength"], json!(500));
        assert_eq!(
            over["summaryLength"],
            json!(over["summary"].as_str().unwrap().chars().count())
        );
    }

    #[tokio::test]
    async fn model_seam_used_over_threshold_and_ignored_below_it() {
        let calls = Arc::new(Mutex::new(Vec::<u64>::new()));
        let calls_for_model = Arc::clone(&calls);
        let model: crate::small_model::ModelSummarySource = Arc::new(move |request| {
            let calls = Arc::clone(&calls_for_model);
            Box::pin(async move {
                calls
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(request.max_length);
                Ok("  model summary  ".to_string())
            })
        });

        // Below threshold: the seam is not even called.
        let under = summarize_text(
            SummarizeParams {
                text: Some("tiny"),
                threshold: 100,
                max_length: Some(80.0),
                mode: SummaryMode::Note,
            },
            Some(Arc::clone(&model)),
        )
        .await;
        assert_eq!(under["summarized"], json!(false));
        assert!(
            calls
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .is_empty()
        );

        // Over threshold: the seam answers with the requested budget.
        let over = summarize_text(
            SummarizeParams {
                text: Some(&"long ".repeat(100)),
                threshold: 10,
                max_length: Some(80.0),
                mode: SummaryMode::Note,
            },
            Some(Arc::clone(&model)),
        )
        .await;
        assert_eq!(over["summarized"], json!(true));
        assert_eq!(over["summary"], json!("model summary"));
        assert!(over.get("reason").is_none());
        assert_eq!(
            calls
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_slice(),
            &[80]
        );

        // Failing seam: exact JS fallback shape.
        let failing: crate::small_model::ModelSummarySource =
            Arc::new(|_request| Box::pin(async { Err("down".to_string()) }));
        let failed = summarize_text(
            SummarizeParams {
                text: Some(&"long ".repeat(100)),
                threshold: 10,
                max_length: Some(80.0),
                mode: SummaryMode::Notification,
            },
            Some(failing),
        )
        .await;
        assert_eq!(failed["summarized"], json!(false));
        assert_eq!(
            failed["reason"],
            json!("Model summarization provider unavailable")
        );
    }
}
