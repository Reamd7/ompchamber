//! Port of the `shared.js` config-layer machinery: JSONC-tolerant
//! config-file reads (user / project / custom layers), deep merge,
//! defensive writes with `.ompchamber.backup` backups, and the per-layer
//! entry-source / write-target resolution used by the agent, command, MCP,
//! and provider CRUD surfaces.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::OpenCodeEnv;

/// Layer identity within [`ConfigLayers`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LayerKind {
    User,
    Project,
    Custom,
}

#[derive(Debug, Clone)]
pub(crate) struct LayerErrorEntry {
    pub path: PathBuf,
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ConfigLayers {
    pub user_config: Map<String, Value>,
    pub project_config: Map<String, Value>,
    pub custom_config: Map<String, Value>,
    pub merged_config: Map<String, Value>,
    pub user_path: PathBuf,
    pub project_path: Option<PathBuf>,
    pub custom_path: Option<PathBuf>,
    pub layer_errors: Vec<LayerErrorEntry>,
}

impl ConfigLayers {
    pub(crate) fn layer(&self, kind: LayerKind) -> &Map<String, Value> {
        match kind {
            LayerKind::User => &self.user_config,
            LayerKind::Project => &self.project_config,
            LayerKind::Custom => &self.custom_config,
        }
    }

    pub(crate) fn layer_mut(&mut self, kind: LayerKind) -> &mut Map<String, Value> {
        match kind {
            LayerKind::User => &mut self.user_config,
            LayerKind::Project => &mut self.project_config,
            LayerKind::Custom => &mut self.custom_config,
        }
    }

    pub(crate) fn layer_path(&self, kind: LayerKind) -> Option<PathBuf> {
        match kind {
            LayerKind::User => Some(self.user_path.clone()),
            LayerKind::Project => self.project_path.clone(),
            LayerKind::Custom => self.custom_path.clone(),
        }
    }

    fn layer_error(&self, path: &Path) -> Option<&LayerErrorEntry> {
        self.layer_errors.iter().find(|entry| entry.path == path)
    }

    /// `throwIfLayerError`.
    fn raise_layer_error(&self, path: &Path) -> Result<(), String> {
        match self.layer_error(path) {
            Some(entry) => Err(entry.message.clone()),
            None => Ok(()),
        }
    }
}

/// `getJsonEntrySource` result.
pub(crate) struct JsonEntrySource {
    pub exists: bool,
    /// Which layer holds the entry (only set when `exists`).
    pub kind: Option<LayerKind>,
    pub path: Option<PathBuf>,
    /// Clone of the section value (`section` in the JS).
    pub section: Option<Value>,
}

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

fn project_config_candidates(working_directory: Option<&Path>) -> Vec<PathBuf> {
    let Some(working_directory) = working_directory else {
        return Vec::new();
    };
    vec![
        working_directory.join("opencode.json"),
        working_directory.join("opencode.jsonc"),
        working_directory.join(".opencode").join("opencode.json"),
        working_directory.join(".opencode").join("opencode.jsonc"),
    ]
}

/// `getProjectConfigPath`: first existing candidate, else the first candidate.
pub(crate) fn project_config_path(working_directory: Option<&Path>) -> Option<PathBuf> {
    let candidates = project_config_candidates(working_directory);
    candidates
        .iter()
        .find(|candidate| candidate.exists())
        .cloned()
        .or_else(|| candidates.first().cloned())
}

/// `getConfigPaths` + `getPrimaryUserConfigPath`.
fn config_paths(env: &OpenCodeEnv, working_directory: Option<&Path>) -> (PathBuf, Option<PathBuf>) {
    let user_paths = [
        env.config_dir.join("config.json"),
        env.config_dir.join("opencode.json"),
        env.config_dir.join("opencode.jsonc"),
    ];
    let user_path = user_paths
        .iter()
        .find(|candidate| candidate.exists())
        .cloned()
        .unwrap_or_else(|| env.config_file());
    (user_path, project_config_path(working_directory))
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

fn format_jsonc_parse_error(file_path: &Path, error: &jsonc_parser::errors::ParseError) -> String {
    let location = format!(" ({:?} at offset {})", error.kind(), error.range().start);
    format!(
        "OpenCode configuration at {} contains invalid JSONC and cannot be loaded safely{location}",
        file_path.display()
    )
}

/// `parseConfigObject` — strict: any parse error or non-object root raises
/// the INVALID_JSONC error instead of returning a partial tree.
fn parse_config_object(content: &str, file_path: &Path) -> Result<Map<String, Value>, String> {
    let options = jsonc_parser::ParseOptions {
        allow_comments: true,
        allow_trailing_commas: true,
        allow_loose_object_property_names: false,
    };
    match jsonc_parser::parse_to_serde_value(content, &options) {
        // Comment-only / whitespace-only files parse to nothing → `{}`.
        Ok(None) => Ok(Map::new()),
        Ok(Some(Value::Object(map))) => Ok(map),
        Ok(Some(_)) => Err(format!(
            "OpenCode configuration at {} contains invalid JSONC and cannot be loaded safely",
            file_path.display()
        )),
        Err(error) => Err(format_jsonc_parse_error(file_path, &error)),
    }
}

/// `readConfigFile` — missing/blank files read as `{}`; JSONC and IO errors
/// raise (the JS maps non-JSONC IO failures to a fixed message).
pub(crate) fn read_config_file(file_path: &Path) -> Result<Map<String, Value>, String> {
    if !file_path.exists() {
        return Ok(Map::new());
    }
    let content = std::fs::read_to_string(file_path).map_err(|error| {
        tracing::error!(
            "Failed to read config file: {}: {error}",
            file_path.display()
        );
        "Failed to read OpenCode configuration".to_string()
    })?;
    let normalized = content.trim();
    if normalized.is_empty() {
        return Ok(Map::new());
    }
    parse_config_object(normalized, file_path)
}

/// `isPlainObject`.
pub(crate) fn is_plain_object(value: &Value) -> bool {
    matches!(value, Value::Object(_))
}

/// `mergeConfigs` — recursive plain-object merge, override wins otherwise.
pub(crate) fn merge_configs(base: &Value, override_value: &Value) -> Value {
    if let (Value::Object(base_map), Value::Object(override_map)) = (base, override_value) {
        let mut result = base_map.clone();
        for (key, value) in override_map {
            let merged = match result.get(key) {
                Some(existing) if is_plain_object(existing) && is_plain_object(value) => {
                    merge_configs(existing, value)
                }
                _ => value.clone(),
            };
            result.insert(key.clone(), merged);
        }
        return Value::Object(result);
    }
    override_value.clone()
}

fn merge_maps(base: &Map<String, Value>, overlay: &Map<String, Value>) -> Map<String, Value> {
    match merge_configs(
        &Value::Object(base.clone()),
        &Value::Object(overlay.clone()),
    ) {
        Value::Object(map) => map,
        other => {
            let mut map = Map::new();
            map.insert("merged".to_string(), other);
            map
        }
    }
}

/// `readConfigLayers`.
pub(crate) fn read_config_layers(
    env: &OpenCodeEnv,
    working_directory: Option<&Path>,
) -> Result<ConfigLayers, String> {
    let (user_path, project_path) = config_paths(env, working_directory);
    let custom_path = env.custom_config.clone();

    let mut layer_errors = Vec::new();
    let (user_config, user_error) = read_layer(Some(&user_path));
    if let Some(message) = user_error {
        layer_errors.push(LayerErrorEntry {
            path: user_path.clone(),
            code: "INVALID_JSONC".to_string(),
            message,
        });
    }
    let (project_config, project_error) = read_layer(project_path.as_deref());
    if let (Some(path), Some(message)) = (project_path.clone(), project_error) {
        layer_errors.push(LayerErrorEntry {
            path,
            code: "INVALID_JSONC".to_string(),
            message,
        });
    }
    let (custom_config, custom_error) = read_layer(custom_path.as_deref());
    if let (Some(path), Some(message)) = (custom_path.clone(), custom_error) {
        layer_errors.push(LayerErrorEntry {
            path,
            code: "INVALID_JSONC".to_string(),
            message,
        });
    }

    let merged = merge_maps(&merge_maps(&user_config, &project_config), &custom_config);

    Ok(ConfigLayers {
        user_config,
        project_config,
        custom_config,
        merged_config: merged,
        user_path,
        project_path,
        custom_path,
        layer_errors,
    })
}

/// `readConfigLayer`: INVALID_JSONC becomes an empty layer + recorded error;
/// any other failure propagates.
fn read_layer(path: Option<&Path>) -> (Map<String, Value>, Option<String>) {
    let Some(path) = path else {
        return (Map::new(), None);
    };
    match read_config_file(path) {
        Ok(config) => (config, None),
        Err(message) => {
            tracing::error!("{message}");
            (Map::new(), Some(message))
        }
    }
}

/// `getConfigForPath`.
pub(crate) fn config_for_path<'a>(
    layers: &'a ConfigLayers,
    target_path: Option<&Path>,
) -> &'a Map<String, Value> {
    let Some(target_path) = target_path else {
        return &layers.user_config;
    };
    if let Some(custom_path) = layers.custom_path.as_deref()
        && target_path == custom_path
    {
        return &layers.custom_config;
    }
    if let Some(project_path) = layers.project_path.as_deref()
        && target_path == project_path
    {
        return &layers.project_config;
    }
    &layers.user_config
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

/// `writeConfig` — never overwrite a file we cannot fully parse; back up the
/// previous file to `<path>.ompchamber.backup` before writing pretty JSON.
pub(crate) fn write_config(config: &Map<String, Value>, file_path: &Path) -> Result<(), String> {
    fn write_failure<T>(_: T) -> String {
        "Failed to write OpenCode configuration".to_string()
    }
    if file_path.exists() {
        let existing = std::fs::read_to_string(file_path).map_err(write_failure)?;
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            parse_config_object(trimmed, file_path)?;
        }
        let backup = backup_path(file_path);
        std::fs::copy(file_path, &backup).map_err(write_failure)?;
        tracing::info!("Created config backup: {}", backup.display());
    }
    if let Some(parent) = file_path.parent() {
        std::fs::create_dir_all(parent).map_err(write_failure)?;
    }
    let serialized =
        serde_json::to_string_pretty(&Value::Object(config.clone())).map_err(write_failure)?;
    std::fs::write(file_path, serialized).map_err(|error| {
        tracing::error!(
            "Failed to write config file {}: {error}",
            file_path.display()
        );
        "Failed to write OpenCode configuration".to_string()
    })?;
    tracing::info!("Successfully wrote config file: {}", file_path.display());
    Ok(())
}

pub(crate) fn backup_path(file_path: &Path) -> PathBuf {
    let mut name = file_path.as_os_str().to_os_string();
    name.push(".ompchamber.backup");
    PathBuf::from(name)
}

// ---------------------------------------------------------------------------
// Entry source / write target
// ---------------------------------------------------------------------------

/// `getJsonEntrySource`: custom → project (unless its layer failed to parse)
/// → user. Raises the recorded layer error for layers it must consult.
pub(crate) fn get_json_entry_source(
    layers: &ConfigLayers,
    section_key: &str,
    entry_name: &str,
) -> Result<JsonEntrySource, String> {
    if let Some(custom_path) = layers.custom_path.clone() {
        layers.raise_layer_error(&custom_path)?;
        if let Some(section) = section_entry(&layers.custom_config, section_key, entry_name) {
            return Ok(JsonEntrySource {
                exists: true,
                kind: Some(LayerKind::Custom),
                path: Some(custom_path),
                section: Some(section),
            });
        }
    }

    if let Some(project_path) = layers.project_path.clone()
        && layers.layer_error(&project_path).is_none()
        && let Some(section) = section_entry(&layers.project_config, section_key, entry_name)
    {
        return Ok(JsonEntrySource {
            exists: true,
            kind: Some(LayerKind::Project),
            path: Some(project_path),
            section: Some(section),
        });
    }

    layers.raise_layer_error(&layers.user_path)?;
    if let Some(section) = section_entry(&layers.user_config, section_key, entry_name) {
        return Ok(JsonEntrySource {
            exists: true,
            kind: Some(LayerKind::User),
            path: Some(layers.user_path.clone()),
            section: Some(section),
        });
    }

    Ok(JsonEntrySource {
        exists: false,
        kind: None,
        path: None,
        section: None,
    })
}

fn section_entry(
    config: &Map<String, Value>,
    section_key: &str,
    entry_name: &str,
) -> Option<Value> {
    config.get(section_key)?.get(entry_name).cloned()
}

/// `getJsonWriteTarget`: custom layer always wins, then project for a
/// project-preferred scope, then the user layer.
pub(crate) fn get_json_write_target(
    layers: &ConfigLayers,
    prefer_project: bool,
) -> Result<(LayerKind, PathBuf), String> {
    if let Some(custom_path) = layers.custom_path.clone() {
        layers.raise_layer_error(&custom_path)?;
        return Ok((LayerKind::Custom, custom_path));
    }
    if prefer_project && let Some(project_path) = layers.project_path.clone() {
        layers.raise_layer_error(&project_path)?;
        return Ok((LayerKind::Project, project_path));
    }
    layers.raise_layer_error(&layers.user_path)?;
    Ok((LayerKind::User, layers.user_path.clone()))
}

/// Mutable view helper: the `config` object the JS handlers receive from
/// `getJsonWriteTarget` / `getJsonEntrySource` is a live layer reference.
/// Rust callers mutate a clone and write it back through this struct.
pub(crate) struct JsonWriteTarget {
    pub kind: LayerKind,
    pub path: PathBuf,
}

/// Section-map mutator mirroring `config.<section>[name]` access: creates
/// missing section/entry maps and errors when an existing entry is not a
/// plain object (JS strict-mode assignment on a primitive throws).
pub(crate) fn entry_map_mut<'a>(
    config: &'a mut Map<String, Value>,
    section_key: &str,
    entry_name: &str,
) -> Result<&'a mut Map<String, Value>, String> {
    let section = config
        .entry(section_key.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let section_map = match section {
        Value::Object(map) => map,
        _ => {
            return Err(format!(
                "Cannot mutate {section_key} section: not an object"
            ));
        }
    };
    let entry = section_map
        .entry(entry_name.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    match entry {
        Value::Object(map) => Ok(map),
        _ => Err(format!(
            "Cannot mutate {section_key}.{entry_name}: not an object"
        )),
    }
}

/// Delete an entry from a section, pruning empty sections
/// (`deleteJsonAgentEntry` semantics generalized).
pub(crate) fn delete_json_entry(
    config: &mut Map<String, Value>,
    section_key: &str,
    entry_name: &str,
) -> bool {
    let Some(Value::Object(section)) = config.get_mut(section_key) else {
        return false;
    };
    if section.remove(entry_name).is_none() {
        return false;
    }
    if section.is_empty() {
        config.remove(section_key);
    }
    true
}

/// Deep-remove a field from `config.<section>[entry]`, pruning the entry and
/// section when they become empty (`delete config.agent[name][field]` chain).
pub(crate) fn delete_entry_field(
    config: &mut Map<String, Value>,
    section_key: &str,
    entry_name: &str,
    field: &str,
) -> bool {
    let Some(Value::Object(section)) = config.get_mut(section_key) else {
        return false;
    };
    let Some(Value::Object(entry)) = section.get_mut(entry_name) else {
        return false;
    };
    if entry.remove(field).is_none() {
        return false;
    }
    if entry.is_empty() {
        section.remove(entry_name);
    }
    if section.is_empty() {
        config.remove(section_key);
    }
    true
}
