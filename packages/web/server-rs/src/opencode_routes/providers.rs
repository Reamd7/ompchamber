//! Port of `opencode/providers.js`: custom provider config validation,
//! persistence into the user/project/custom JSONC layers, and source
//! reporting. Secrets stay in `auth.json` (handled by `auth.rs`).

use std::path::PathBuf;

use serde_json::{Map, Value, json};

use super::OpenCodeEnv;
use super::config_layers::{config_for_path, is_plain_object, read_config_layers, write_config};

/// `^[a-z0-9][a-z0-9-_]*$`.
fn valid_provider_id(provider_id: &str) -> bool {
    let bytes = provider_id.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let head = bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit();
    let tail = bytes
        .iter()
        .skip(1)
        .all(|&b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    head && tail
}

fn custom_npm_allowed(npm: &str) -> bool {
    matches!(
        npm,
        "@ai-sdk/openai-compatible" | "@ai-sdk/openai" | "@ai-sdk/anthropic"
    )
}

/// `getProviderSources` — returns the inner `sources` object
/// (`{ auth, user, project, custom }`); `auth.exists` is filled in by the
/// route (auth.json / Claude CLI probe).
pub(crate) fn get_provider_sources(
    env: &OpenCodeEnv,
    provider_id: &str,
    working_directory: Option<&std::path::Path>,
) -> Result<Value, String> {
    let layers = read_config_layers(env, working_directory)?;

    let has = |config: &Map<String, Value>, key: &str| {
        config.get(key).map(is_plain_object).unwrap_or(false)
            && config
                .get(key)
                .and_then(|section| section.get(provider_id))
                .is_some()
    };

    let custom_exists =
        has(&layers.custom_config, "provider") || has(&layers.custom_config, "providers");
    let project_exists =
        has(&layers.project_config, "provider") || has(&layers.project_config, "providers");
    let user_exists = has(&layers.user_config, "provider") || has(&layers.user_config, "providers");

    Ok(json!({
        "auth": { "exists": false },
        "user": { "exists": user_exists, "path": layers.user_path.to_string_lossy() },
        "project": {
            "exists": project_exists,
            "path": layers.project_path.as_ref().map(|p| json!(p.to_string_lossy())).unwrap_or(Value::Null),
        },
        "custom": {
            "exists": custom_exists,
            "path": layers.custom_path.as_ref().map(|p| json!(p.to_string_lossy())).unwrap_or(Value::Null),
        },
    }))
}
/// `validateCustomProviderConfig` outcome.
#[derive(Debug)]
pub(crate) enum ProviderValidation {
    Ok { provider_id: String, config: Value },
    Err(String),
}

/// `validateCustomProviderConfig`.
pub(crate) fn validate_custom_provider_config(
    provider_id: &str,
    config: &Value,
    has_stored_auth: bool,
) -> ProviderValidation {
    if provider_id.is_empty() || !valid_provider_id(provider_id) {
        return ProviderValidation::Err(
            "Provider ID must match /^[a-z0-9][a-z0-9-_]*$/".to_string(),
        );
    }
    let Some(config_map) = config.as_object() else {
        return ProviderValidation::Err("Provider config must be an object".to_string());
    };

    let name = config_map
        .get("name")
        .and_then(Value::as_str)
        .map(|n| n.trim())
        .unwrap_or_default();
    if name.is_empty() {
        return ProviderValidation::Err("Provider name is required".to_string());
    }

    let npm = match config_map.get("npm") {
        Some(Value::String(npm)) => npm.trim(),
        _ => "@ai-sdk/openai-compatible",
    };
    if !custom_npm_allowed(npm) {
        return ProviderValidation::Err(
            "Custom providers must use @ai-sdk/openai-compatible, @ai-sdk/openai, or @ai-sdk/anthropic"
                .to_string(),
        );
    }

    let Some(options_block) = config_map.get("options").filter(|v| is_plain_object(v)) else {
        return ProviderValidation::Err("Provider options are required".to_string());
    };

    let base_url = options_block
        .get("baseURL")
        .and_then(Value::as_str)
        .map(|u| u.trim())
        .unwrap_or_default();
    if base_url.is_empty() {
        return ProviderValidation::Err("Base URL is required".to_string());
    }
    if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
        return ProviderValidation::Err("Base URL must start with http:// or https://".to_string());
    }

    let Some(models) = config_map.get("models").filter(|v| is_plain_object(v)) else {
        return ProviderValidation::Err("At least one model is required".to_string());
    };
    let Some(Value::Object(models_map)) = Some(models) else {
        return ProviderValidation::Err("At least one model is required".to_string());
    };
    if models_map.is_empty() {
        return ProviderValidation::Err("At least one model is required".to_string());
    }

    let mut normalized_models = Map::new();
    for (model_id, model_value) in models_map {
        let trimmed_id = model_id.trim();
        if trimmed_id.is_empty() {
            return ProviderValidation::Err("Model id is required".to_string());
        }
        if !is_plain_object(model_value) {
            return ProviderValidation::Err(format!("Model \"{trimmed_id}\" must be an object"));
        }
        let model_name = model_value
            .get("name")
            .and_then(Value::as_str)
            .map(|n| n.trim())
            .unwrap_or_default();
        if model_name.is_empty() {
            return ProviderValidation::Err(format!("Model \"{trimmed_id}\" requires a name"));
        }
        normalized_models.insert(trimmed_id.to_string(), json!({ "name": model_name }));
    }

    let mut normalized = Map::new();
    normalized.insert("npm".to_string(), json!(npm));
    normalized.insert("name".to_string(), json!(name));
    normalized.insert("options".to_string(), json!({ "baseURL": base_url }));
    normalized.insert("models".to_string(), Value::Object(normalized_models));

    let mut env: Vec<Value> = Vec::new();
    if let Some(Value::Array(entries)) = config_map.get("env") {
        env = entries
            .iter()
            .filter_map(|entry| {
                let text = entry.as_str()?.trim().to_string();
                if text.is_empty() {
                    None
                } else {
                    Some(Value::String(text))
                }
            })
            .collect();
        if !env.is_empty() {
            normalized.insert("env".to_string(), Value::Array(env.clone()));
        }
    }

    if env.is_empty() && !has_stored_auth {
        return ProviderValidation::Err(
            "API key or {env:VAR} credentials are required".to_string(),
        );
    }

    if let Some(Value::Object(headers)) = options_block.get("headers") {
        let mut cleaned = Map::new();
        for (header_key, header_value) in headers {
            if header_key.trim().is_empty() {
                continue;
            }
            let text = match header_value {
                Value::String(text) => text.trim(),
                _ => {
                    return ProviderValidation::Err(format!(
                        "Header \"{header_key}\" requires a non-empty value"
                    ));
                }
            };
            if text.is_empty() {
                return ProviderValidation::Err(format!(
                    "Header \"{header_key}\" requires a non-empty value"
                ));
            }
            cleaned.insert(header_key.trim().to_string(), json!(text));
        }
        if !cleaned.is_empty()
            && let Some(Value::Object(options)) = normalized.get_mut("options")
        {
            options.insert("headers".to_string(), Value::Object(cleaned));
        }
    }

    ProviderValidation::Ok {
        provider_id: provider_id.to_string(),
        config: Value::Object(normalized),
    }
}

/// `upsertProviderConfig` outcome (the `statusCode: 400` shape of the JS
/// validation error is reproduced by the `Validation` variant).
pub(crate) struct UpsertOutcome {
    pub provider_id: String,
    pub path: PathBuf,
    pub config: Value,
}

#[derive(Debug)]
pub(crate) enum UpsertError {
    Validation(String),
    Other(String),
}

/// `upsertProviderConfig`.
pub(crate) fn upsert_provider_config(
    env: &OpenCodeEnv,
    provider_id: &str,
    config: &Value,
    working_directory: Option<&std::path::Path>,
    scope: &str,
    has_stored_auth: bool,
) -> Result<UpsertOutcome, UpsertError> {
    let validated = match validate_custom_provider_config(provider_id, config, has_stored_auth) {
        ProviderValidation::Ok {
            provider_id,
            config,
        } => (provider_id, config),
        ProviderValidation::Err(message) => return Err(UpsertError::Validation(message)),
    };

    let layers = read_config_layers(env, working_directory).map_err(UpsertError::Other)?;
    let mut target_path = layers.user_path.clone();

    match scope {
        "project" => {
            let Some(working_directory) = working_directory else {
                return Err(UpsertError::Other(
                    "Working directory is required for project scope".to_string(),
                ));
            };
            let _ = working_directory;
            target_path = layers.project_path.clone().unwrap_or(target_path);
        }
        "custom" => {
            let Some(custom_path) = layers.custom_path.clone() else {
                return Err(UpsertError::Other(
                    "Custom config path (OPENCODE_CONFIG) is not set".to_string(),
                ));
            };
            target_path = custom_path;
        }
        "user" => {}
        other => return Err(UpsertError::Other(format!("Invalid scope: {other}"))),
    }

    let mut target_config = config_for_path(&layers, Some(&target_path)).clone();
    let mut provider_map = match target_config.get("provider") {
        Some(value) if is_plain_object(value) => match value {
            Value::Object(map) => map.clone(),
            _ => Map::new(),
        },
        _ => Map::new(),
    };
    provider_map.insert(validated.0.clone(), validated.1.clone());
    target_config.insert("provider".to_string(), Value::Object(provider_map));

    if let Some(Value::Array(disabled)) = target_config.get_mut("disabled_providers") {
        disabled.retain(|entry| entry != &json!(validated.0));
    }

    let write_path = if target_path.as_os_str().is_empty() {
        env.config_file()
    } else {
        target_path
    };
    write_config(&target_config, &write_path).map_err(UpsertError::Other)?;

    Ok(UpsertOutcome {
        provider_id: validated.0,
        path: write_path,
        config: validated.1,
    })
}

/// `removeProviderConfig`.
pub(crate) fn remove_provider_config(
    env: &OpenCodeEnv,
    provider_id: &str,
    working_directory: Option<&std::path::Path>,
    scope: &str,
) -> Result<bool, String> {
    if provider_id.is_empty() {
        return Err("Provider ID is required".to_string());
    }

    let layers = read_config_layers(env, working_directory)?;
    let mut target_path = layers.user_path.clone();

    match scope {
        "project" => {
            if working_directory.is_none() {
                return Err("Working directory is required for project scope".to_string());
            }
            target_path = layers.project_path.clone().unwrap_or(target_path);
        }
        "custom" => {
            let Some(custom_path) = layers.custom_path.clone() else {
                return Ok(false);
            };
            target_path = custom_path;
        }
        _ => {}
    }

    let mut target_config = config_for_path(&layers, Some(&target_path)).clone();
    let provider_map = target_config
        .get("provider")
        .filter(|v| is_plain_object(v))
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    let providers_map = target_config
        .get("providers")
        .filter(|v| is_plain_object(v))
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();

    let removed_provider = provider_map.contains_key(provider_id);
    let removed_providers = providers_map.contains_key(provider_id);
    if !removed_provider && !removed_providers {
        return Ok(false);
    }

    if removed_provider {
        let mut next = provider_map.clone();
        next.remove(provider_id);
        if next.is_empty() {
            target_config.remove("provider");
        } else {
            target_config.insert("provider".to_string(), Value::Object(next));
        }
    }
    if removed_providers {
        let mut next = providers_map.clone();
        next.remove(provider_id);
        if next.is_empty() {
            target_config.remove("providers");
        } else {
            target_config.insert("providers".to_string(), Value::Object(next));
        }
    }

    let write_path = if target_path.as_os_str().is_empty() {
        env.config_file()
    } else {
        target_path
    };
    write_config(&target_config, &write_path)?;
    tracing::info!(
        "Removed provider {provider_id} from config: {}",
        write_path.display()
    );
    Ok(true)
}
