//! Minimal port of the config-layer reader from
//! `server/lib/opencode/shared.js` — only the surface small-model consumes:
//! `readConfigLayers` / `readConfig` (user ← project ← custom merge, JSONC),
//! `isPlainObject`, and the paths each layer came from (needed to resolve
//! relative `{file:…}` references against the layer that declared them).
//!
//! JSONC layers that fail to parse read as `{}` (JS `readConfigLayer` catches
//! the INVALID_JSONC error and logs); everything else is a hard error.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

#[derive(Debug, Clone, Default)]
pub struct ConfigLayers {
    pub user_config: Value,
    pub project_config: Value,
    pub custom_config: Value,
    pub merged: Value,
    pub user_path: Option<PathBuf>,
    pub project_path: Option<PathBuf>,
    pub custom_path: Option<PathBuf>,
}

pub fn is_plain_object(value: &Value) -> bool {
    value.as_object().is_some()
}

fn opencode_config_dir() -> PathBuf {
    crate::config::home_dir()
        .map(|home| home.join(".config").join("opencode"))
        // No home: JS would have thrown long before reaching config reads.
        .unwrap_or_else(|| PathBuf::from(".config/opencode"))
}

fn user_config_candidates() -> Vec<PathBuf> {
    let dir = opencode_config_dir();
    vec![
        dir.join("config.json"),
        dir.join("opencode.json"),
        dir.join("opencode.jsonc"),
    ]
}

fn project_config_candidates(working_directory: &Path) -> Vec<PathBuf> {
    vec![
        working_directory.join("opencode.json"),
        working_directory.join("opencode.jsonc"),
        working_directory.join(".opencode").join("opencode.json"),
        working_directory.join(".opencode").join("opencode.jsonc"),
    ]
}

fn primary_user_config_path() -> PathBuf {
    user_config_candidates()
        .into_iter()
        .find(|candidate| candidate.exists())
        .unwrap_or_else(|| opencode_config_dir().join("config.json"))
}

fn project_config_path(working_directory: &Path) -> Option<PathBuf> {
    let candidates = project_config_candidates(working_directory);
    candidates
        .iter()
        .find(|candidate| candidate.exists())
        .cloned()
        // JS returns candidates[0] even when it does not exist (reads as {}).
        .or_else(|| candidates.into_iter().next())
}

fn custom_config_path() -> Option<PathBuf> {
    std::env::var("OPENCODE_CONFIG")
        .ok()
        .map(|value| PathBuf::from(value.trim()))
        .filter(|value| !value.as_os_str().is_empty())
        .map(|value| {
            if value.is_absolute() {
                value
            } else {
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join(value)
            }
        })
}

/// `readConfigFile`: missing/blank → `{}`; JSONC parse of non-object or
/// invalid content reads as an empty layer (mirroring `readConfigLayer`'s
/// INVALID_JSONC containment for this module's needs).
fn read_config_layer_file(path: Option<&Path>) -> Value {
    let Some(path) = path else {
        return Value::Object(Map::new());
    };
    if !path.exists() {
        return Value::Object(Map::new());
    }
    let Ok(content) = std::fs::read_to_string(path) else {
        return Value::Object(Map::new());
    };
    if content.trim().is_empty() {
        return Value::Object(Map::new());
    }
    let options = jsonc_parser::ParseOptions {
        allow_comments: true,
        allow_loose_object_property_names: false,
        allow_trailing_commas: true,
    };
    match jsonc_parser::parse_to_serde_value(&content, &options) {
        Ok(Some(Value::Object(map))) => Value::Object(map),
        Ok(_) => Value::Object(Map::new()),
        Err(error) => {
            tracing::warn!(
                "OpenCode configuration at {} contains invalid JSONC and cannot be loaded safely: {error}",
                path.display()
            );
            Value::Object(Map::new())
        }
    }
}

/// `mergeConfigs`: plain objects deep-merge; any non-object override replaces.
pub fn merge_configs(base: &Value, over: &Value) -> Value {
    let (Some(base_map), Some(over_map)) = (base.as_object(), over.as_object()) else {
        return over.clone();
    };
    let mut result = base_map.clone();
    for (key, value) in over_map {
        let merged = match result.get(key) {
            Some(existing) if is_plain_object(existing) && is_plain_object(value) => {
                merge_configs(existing, value)
            }
            _ => value.clone(),
        };
        result.insert(key.clone(), merged);
    }
    Value::Object(result)
}

/// Injectable seam (the JS tests mock `../opencode/shared.js` wholesale).
pub type ConfigReader = std::sync::Arc<dyn Fn(Option<&str>) -> ConfigLayers + Send + Sync>;

pub fn read_config_layers(working_directory: Option<&str>) -> ConfigLayers {
    let user_path = primary_user_config_path();
    let project_path = working_directory
        .filter(|value| !value.is_empty())
        .map(Path::new)
        .and_then(project_config_path);
    let custom_path = custom_config_path();

    let user_config = read_config_layer_file(Some(&user_path));
    let project_config = read_config_layer_file(project_path.as_deref());
    let custom_config = read_config_layer_file(custom_path.as_deref());
    let merged = merge_configs(
        &merge_configs(&user_config, &project_config),
        &custom_config,
    );

    ConfigLayers {
        user_config,
        project_config,
        custom_config,
        merged,
        user_path: Some(user_path),
        project_path,
        custom_path,
    }
}

pub fn read_config(working_directory: Option<&str>) -> Value {
    read_config_layers(working_directory).merged
}

/// Production filesystem reader.
pub fn fs_config_reader() -> ConfigReader {
    std::sync::Arc::new(|working_directory: Option<&str>| read_config_layers(working_directory))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn merge_deep_merges_objects_and_replaces_scalars() {
        let base = json!({ "a": { "x": 1, "y": 2 }, "b": "keep", "c": 1 });
        let over = json!({ "a": { "y": 3 }, "b": null, "d": [1] });
        assert_eq!(
            merge_configs(&base, &over),
            json!({ "a": { "x": 1, "y": 3 }, "b": null, "c": 1, "d": [1] })
        );
    }

    #[test]
    fn non_object_override_replaces_whole_branch() {
        assert_eq!(
            merge_configs(&json!({ "a": { "x": 1 } }), &json!({ "a": 5 })),
            json!({ "a": 5 })
        );
    }

    #[test]
    fn jsonc_layers_parse_with_comments_and_trailing_commas() {
        let dir =
            std::env::temp_dir().join(format!("sm-cfg-{}", crate::small_model::http::now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("opencode.json"),
            "{\n  // provider config\n  \"small_model\": \"anthropic/claude-haiku-4-5\",\n}",
        )
        .unwrap();
        let layers = read_config_layers(dir.to_str());
        assert_eq!(layers.project_path, Some(dir.join("opencode.json")));
        assert_eq!(
            layers.merged["small_model"],
            json!("anthropic/claude-haiku-4-5")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn invalid_layer_reads_as_empty() {
        let dir =
            std::env::temp_dir().join(format!("sm-cfg-bad-{}", crate::small_model::http::now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("opencode.json"), "{ truncated").unwrap();
        let layers = read_config_layers(dir.to_str());
        assert_eq!(layers.project_config, json!({}));
        // The broken layer contributes nothing to the merge: user-config
        // values survive untouched.
        let without_project = read_config_layers(None);
        assert_eq!(layers.merged, without_project.merged);
        std::fs::remove_dir_all(&dir).ok();
    }
}
