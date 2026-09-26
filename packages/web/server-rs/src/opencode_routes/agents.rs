//! Port of `opencode/agents.js`: agent config CRUD across project/user `.md`
//! files and the JSONC config layers, with the permission-source resolution
//! and built-in-override semantics of the JS.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use super::OpenCodeEnv;
use super::config_layers::{
    LayerKind, delete_entry_field, delete_json_entry, entry_map_mut, get_json_entry_source,
    get_json_write_target, read_config_file, read_config_layers, write_config,
};
use super::md_file::{
    MdFile, ensure_dirs, is_prompt_file_reference, parse_md_file, resolve_prompt_file_path,
    write_md_file, write_prompt_file,
};
use super::webutil::{js_to_string, js_truthy, object_keys};

pub(crate) type AgentResult<T> = Result<T, String>;

pub(crate) const SCOPE_PROJECT: &str = "project";
pub(crate) const SCOPE_USER: &str = "user";

// ---------------------------------------------------------------------------
// Scope helpers
// ---------------------------------------------------------------------------

/// `ensureProjectAgentDir` — creates both the plural and legacy dirs.
fn ensure_project_agent_dir(working_directory: &Path) -> PathBuf {
    let project_agent_dir = working_directory.join(".opencode").join("agents");
    if !project_agent_dir.exists() {
        let _ = std::fs::create_dir_all(&project_agent_dir);
    }
    let legacy = working_directory.join(".opencode").join("agent");
    if !legacy.exists() {
        let _ = std::fs::create_dir_all(&legacy);
    }
    project_agent_dir
}

/// `getProjectAgentPath` — plural path unless only the legacy file exists.
pub(crate) fn project_agent_path(working_directory: &Path, agent_name: &str) -> PathBuf {
    let plural = working_directory
        .join(".opencode")
        .join("agents")
        .join(format!("{agent_name}.md"));
    let legacy = working_directory
        .join(".opencode")
        .join("agent")
        .join(format!("{agent_name}.md"));
    if legacy.exists() && !plural.exists() {
        return legacy;
    }
    plural
}

/// `buildUserAgentIndex`: walk AGENT_DIR depth-first; within each directory
/// entries are processed in name order and the first `.md` wins per name.
#[derive(Default)]
struct UserAgentIndex {
    by_name: BTreeMap<String, PathBuf>,
}

fn build_user_agent_index(env: &OpenCodeEnv) -> UserAgentIndex {
    let mut index = UserAgentIndex::default();
    let root = env.agent_dir();
    if !root.is_dir() {
        return index;
    }
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut names: Vec<(String, bool)> = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = entry.path().is_dir();
            names.push((name, is_dir));
        }
        names.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, is_dir) in &names {
            if !*is_dir && !name.ends_with(".md") {
                continue;
            }
            if *is_dir {
                continue;
            }
            let agent_name = name.strip_suffix(".md").unwrap_or(name);
            index
                .by_name
                .entry(agent_name.to_string())
                .or_insert_with(|| dir.join(name));
        }
        // Push subdirectories reversed so the stack pops them in name order.
        let mut dirs: Vec<PathBuf> = names
            .iter()
            .filter(|(_, is_dir)| *is_dir)
            .map(|(name, _)| dir.join(name))
            .collect();
        dirs.reverse();
        stack.extend(dirs);
    }
    index
}

/// `getUserAgentPath`: flat → legacy → indexed subfolder → flat default.
pub(crate) fn user_agent_path(
    env: &OpenCodeEnv,
    agent_name: &str,
    index: &UserAgentIndex,
) -> PathBuf {
    let plural = env.agent_dir().join(format!("{agent_name}.md"));
    if plural.exists() {
        return plural;
    }
    let legacy = env
        .config_dir
        .join("agent")
        .join(format!("{agent_name}.md"));
    if legacy.exists() {
        return legacy;
    }
    if let Some(found) = index.by_name.get(agent_name) {
        return found.clone();
    }
    plural
}

fn agent_index(env: &OpenCodeEnv) -> UserAgentIndex {
    build_user_agent_index(env)
}

/// `getAgentScope`.
fn agent_scope(
    env: &OpenCodeEnv,
    agent_name: &str,
    working_directory: Option<&Path>,
    index: &UserAgentIndex,
) -> (Option<&'static str>, Option<PathBuf>) {
    if let Some(working_directory) = working_directory {
        let project_path = project_agent_path(working_directory, agent_name);
        if project_path.exists() {
            return (Some(SCOPE_PROJECT), Some(project_path));
        }
    }
    let user_path = user_agent_path(env, agent_name, index);
    if user_path.exists() {
        return (Some(SCOPE_USER), Some(user_path));
    }
    (None, None)
}

/// `getAgentWritePath`.
fn agent_write_path(
    env: &OpenCodeEnv,
    agent_name: &str,
    working_directory: Option<&Path>,
    requested_scope: Option<&str>,
    index: &UserAgentIndex,
) -> (&'static str, PathBuf) {
    if let (Some(scope), Some(path)) = agent_scope(env, agent_name, working_directory, index) {
        return (scope, path);
    }
    if requested_scope == Some(SCOPE_PROJECT)
        && let Some(working_directory) = working_directory
    {
        return (
            SCOPE_PROJECT,
            project_agent_path(working_directory, agent_name),
        );
    }
    (SCOPE_USER, user_agent_path(env, agent_name, index))
}

/// `getAgentPermissionSource`: project .md → user .md → custom/project/user
/// JSON layers.
fn agent_permission_source(
    env: &OpenCodeEnv,
    agent_name: &str,
    working_directory: Option<&Path>,
    index: &UserAgentIndex,
) -> AgentResult<(Option<&'static str>, Option<&'static str>, Option<PathBuf>)> {
    if let Some(working_directory) = working_directory {
        let project_md = project_agent_path(working_directory, agent_name);
        if project_md.exists()
            && let Ok(md) = parse_md_file(&project_md)
            && md.frontmatter.contains_key("permission")
        {
            return Ok((Some("md"), Some(SCOPE_PROJECT), Some(project_md)));
        }
    }

    let user_md = user_agent_path(env, agent_name, index);
    if user_md.exists()
        && let Ok(md) = parse_md_file(&user_md)
        && md.frontmatter.contains_key("permission")
    {
        return Ok((Some("md"), Some(SCOPE_USER), Some(user_md)));
    }

    // Fresh layer read, exactly like the JS. `?.permission !== undefined`
    // is a presence check on the entry object (a JSON `null` counts).
    let layers = read_config_layers(env, working_directory)?;

    if let Some(custom_path) = layers.custom_path.clone()
        && layer_defines_permission(&layers.custom_config, agent_name)
    {
        return Ok((Some("json"), Some("custom"), Some(custom_path)));
    }
    if let Some(project_path) = layers.project_path.clone()
        && layer_defines_permission(&layers.project_config, agent_name)
    {
        return Ok((Some("json"), Some(SCOPE_PROJECT), Some(project_path)));
    }
    if layer_defines_permission(&layers.user_config, agent_name) {
        return Ok((
            Some("json"),
            Some(SCOPE_USER),
            Some(layers.user_path.clone()),
        ));
    }

    Ok((None, None, None))
}

/// `config.agent[name].permission !== undefined` — the section must be an
/// object, the entry must be an object, and the field must be present.
fn layer_defines_permission(config: &Map<String, Value>, agent_name: &str) -> bool {
    let Some(Value::Object(section)) = config.get("agent") else {
        return false;
    };
    let Some(Value::Object(entry)) = section.get(agent_name) else {
        return false;
    };
    entry.contains_key("permission")
}

/// `applyAgentPermission` on a frontmatter/entry map.
fn apply_agent_permission(target: &mut Map<String, Value>, new_permission: Option<Value>) {
    match new_permission {
        None => {
            target.remove("permission");
        }
        Some(value) => {
            target.insert("permission".to_string(), value);
        }
    }
}

// ---------------------------------------------------------------------------
// Read APIs
// ---------------------------------------------------------------------------

/// `getAgentSources`.
pub(crate) fn get_agent_sources(
    env: &OpenCodeEnv,
    agent_name: &str,
    working_directory: Option<&Path>,
) -> AgentResult<Value> {
    let index = agent_index(env);
    let project_path = working_directory.map(|wd| project_agent_path(wd, agent_name));
    let project_exists = project_path.as_ref().is_some_and(|p| p.exists());

    let user_path = user_agent_path(env, agent_name, &index);
    let user_exists = user_path.exists();

    let md_path = if project_exists {
        project_path.clone()
    } else if user_exists {
        Some(user_path.clone())
    } else {
        None
    };
    let md_exists = md_path.is_some();
    let md_scope = if project_exists {
        json!(SCOPE_PROJECT)
    } else if user_exists {
        json!(SCOPE_USER)
    } else {
        Value::Null
    };

    let layers = read_config_layers(env, working_directory)?;
    let json_source = get_json_entry_source(&layers, "agent", agent_name)?;
    let json_section = json_source.section.clone();
    let json_path = json_source
        .path
        .clone()
        .or_else(|| layers.custom_path.clone())
        .or_else(|| layers.project_path.clone())
        .or_else(|| Some(layers.user_path.clone()));
    let json_scope = if json_source.path == layers.project_path {
        json!(SCOPE_PROJECT)
    } else {
        json!(SCOPE_USER)
    };

    let mut md_fields: Vec<Value> = Vec::new();
    if let Some(md_path) = md_path.as_deref() {
        let md = parse_md_file(md_path)?;
        md_fields = object_keys(&Value::Object(md.frontmatter))
            .into_iter()
            .map(Value::String)
            .collect();
        if !md.body.is_empty() {
            md_fields.push(Value::String("prompt".to_string()));
        }
    }

    let mut json_fields: Vec<Value> = Vec::new();
    if let Some(section) = json_section.as_ref() {
        json_fields = object_keys(section)
            .into_iter()
            .map(Value::String)
            .collect();
    }

    Ok(json!({
        "md": {
            "exists": md_exists,
            "path": path_json(&md_path),
            "scope": md_scope,
            "fields": md_fields,
        },
        "json": {
            "exists": json_source.exists,
            "path": path_json(&json_path),
            "scope": if json_source.exists { json_scope } else { Value::Null },
            "fields": json_fields,
        },
        "projectMd": {
            "exists": project_exists,
            "path": path_json(&project_path),
        },
        "userMd": {
            "exists": user_exists,
            "path": user_path_to_value(&user_path),
        },
    }))
}

fn path_json(path: &Option<PathBuf>) -> Value {
    match path {
        Some(path) => json!(path.to_string_lossy()),
        None => Value::Null,
    }
}

fn user_path_to_value(path: &Path) -> Value {
    json!(path.to_string_lossy())
}

/// `getAgentConfig`.
pub(crate) fn get_agent_config(
    env: &OpenCodeEnv,
    agent_name: &str,
    working_directory: Option<&Path>,
) -> AgentResult<Value> {
    let index = agent_index(env);
    let project_path = working_directory.map(|wd| project_agent_path(wd, agent_name));
    let project_exists = project_path.as_ref().is_some_and(|p| p.exists());
    let user_path = user_agent_path(env, agent_name, &index);
    let user_exists = user_path.exists();

    if project_exists || user_exists {
        let md_path = if project_exists {
            project_path.clone().unwrap_or_default()
        } else {
            user_path
        };
        let md = parse_md_file(&md_path)?;
        let mut config = md.frontmatter;
        if !md.body.is_empty() {
            config.insert("prompt".to_string(), Value::String(md.body.clone()));
        }
        return Ok(json!({
            "source": "md",
            "scope": if project_exists { SCOPE_PROJECT } else { SCOPE_USER },
            "config": Value::Object(config),
        }));
    }

    let layers = read_config_layers(env, working_directory)?;
    let json_source = get_json_entry_source(&layers, "agent", agent_name)?;
    if json_source.exists
        && let Some(section) = json_source.section.filter(|value| value.is_object())
    {
        let scope = if json_source.path == layers.project_path {
            SCOPE_PROJECT
        } else {
            SCOPE_USER
        };
        return Ok(json!({
            "source": "json",
            "scope": scope,
            "config": section,
        }));
    }

    Ok(json!({
        "source": "none",
        "scope": Value::Null,
        "config": {},
    }))
}

// ---------------------------------------------------------------------------
// Write APIs
// ---------------------------------------------------------------------------

/// `createAgent`.
pub(crate) fn create_agent(
    env: &OpenCodeEnv,
    agent_name: &str,
    config: &Map<String, Value>,
    working_directory: Option<&Path>,
    scope: Option<&str>,
) -> AgentResult<()> {
    ensure_dirs(env);
    let index = agent_index(env);

    let project_path = working_directory.map(|wd| project_agent_path(wd, agent_name));
    let user_path = user_agent_path(env, agent_name, &index);

    if let Some(project_path) = project_path.as_ref()
        && project_path.exists()
    {
        return Err(format!(
            "Agent {agent_name} already exists as project-level .md file"
        ));
    }
    if user_path.exists() {
        return Err(format!(
            "Agent {agent_name} already exists as user-level .md file"
        ));
    }

    let layers = read_config_layers(env, working_directory)?;
    if get_json_entry_source(&layers, "agent", agent_name)?.exists {
        return Err(format!(
            "Agent {agent_name} already exists in opencode.json"
        ));
    }

    let (target_path, target_scope) = if scope == Some(SCOPE_PROJECT)
        && let Some(working_directory) = working_directory
    {
        (
            project_agent_path(working_directory, agent_name),
            SCOPE_PROJECT,
        )
    } else {
        (user_path, SCOPE_USER)
    };
    if scope == Some(SCOPE_PROJECT) && working_directory.is_some() {
        ensure_project_agent_dir(working_directory.unwrap());
    }

    // `const { prompt, scope, ...rawFrontmatter } = config` minus null values.
    let mut frontmatter: Map<String, Value> = Map::new();
    for (key, value) in config {
        if key == "prompt" || key == "scope" || value.is_null() {
            continue;
        }
        frontmatter.insert(key.clone(), value.clone());
    }
    let prompt = config
        .get("prompt")
        .and_then(Value::as_str)
        .unwrap_or_default();

    write_md_file(&target_path, &frontmatter, prompt)?;
    tracing::info!(
        "Created new agent: {agent_name} (scope: {target_scope}, path: {})",
        target_path.display()
    );
    Ok(())
}

/// `updateAgent`.
pub(crate) fn update_agent(
    env: &OpenCodeEnv,
    agent_name: &str,
    updates: &Map<String, Value>,
    working_directory: Option<&Path>,
) -> AgentResult<()> {
    ensure_dirs(env);
    let index = agent_index(env);

    let (scope, md_path) = agent_write_path(env, agent_name, working_directory, None, &index);
    let md_exists = md_path.exists();

    let layers = read_config_layers(env, working_directory)?;
    let json_source = get_json_entry_source(&layers, "agent", agent_name)?;
    let json_section = json_source.section.clone();
    let has_json_fields = json_source.exists
        && json_section
            .as_ref()
            .is_some_and(|section| !object_keys(section).is_empty());
    let (target_kind, json_target_path) = if json_source.exists {
        (
            json_source.kind.unwrap_or(LayerKind::User),
            json_source.path.unwrap_or_else(|| layers.user_path.clone()),
        )
    } else {
        get_json_write_target(&layers, false)?
    };
    // The JS mutates the live layer object through `config`; we mutate a
    // clone and write it back when `jsonModified` is set.
    let mut config = layers.layer(target_kind).clone();

    let is_builtin_override = !md_exists && !has_json_fields;

    let mut target_path = md_path.clone();
    let mut target_scope = scope.to_string();
    if !md_exists && is_builtin_override {
        target_path = user_agent_path(env, agent_name, &index);
        target_scope = SCOPE_USER.to_string();
    }

    let mut md_data: Option<MdFile> = if md_exists {
        Some(parse_md_file(&md_path)?)
    } else if is_builtin_override {
        Some(MdFile {
            frontmatter: Map::new(),
            body: String::new(),
        })
    } else {
        None
    };

    let mut md_modified = false;
    let mut json_modified = false;
    let creating_new_md = is_builtin_override;

    for (field, value) in updates {
        if field == "prompt" {
            if value.is_null() {
                if md_exists || creating_new_md {
                    if let Some(md) = md_data.as_mut() {
                        md.body = String::new();
                        md_modified = true;
                    }
                    continue;
                }

                let section_prompt = json_section
                    .as_ref()
                    .and_then(|section| section.get("prompt").cloned())
                    .unwrap_or(Value::Null);
                if is_prompt_file_reference(&section_prompt) {
                    let prompt_path =
                        resolve_prompt_file_path(env, &section_prompt).ok_or_else(|| {
                            format!("Invalid prompt file reference for agent {agent_name}")
                        })?;
                    write_prompt_file(&prompt_path, "");
                    continue;
                }

                if config
                    .get("agent")
                    .and_then(|section| section.get(agent_name))
                    .is_some_and(js_truthy)
                {
                    delete_entry_field(&mut config, "agent", agent_name, "prompt");
                    json_modified = true;
                }
                continue;
            }

            let normalized = match value {
                Value::String(text) => text.clone(),
                other => js_to_string(other),
            };

            if md_exists || creating_new_md {
                if let Some(md) = md_data.as_mut() {
                    md.body = normalized;
                    md_modified = true;
                }
                continue;
            }
            let section_prompt = json_section
                .as_ref()
                .and_then(|section| section.get("prompt").cloned())
                .unwrap_or(Value::Null);
            if is_prompt_file_reference(&section_prompt) {
                let prompt_path =
                    resolve_prompt_file_path(env, &section_prompt).ok_or_else(|| {
                        format!("Invalid prompt file reference for agent {agent_name}")
                    })?;
                write_prompt_file(&prompt_path, &normalized);
                continue;
            } else if is_prompt_file_reference(&Value::String(normalized.clone())) {
                entry_map_mut(&mut config, "agent", agent_name)?
                    .insert("prompt".to_string(), Value::String(normalized));
                json_modified = true;
                continue;
            }

            entry_map_mut(&mut config, "agent", agent_name)?
                .insert("prompt".to_string(), Value::String(normalized));
            json_modified = true;
            continue;
        }

        if field == "permission" {
            let (source, _source_scope, source_path) =
                agent_permission_source(env, agent_name, working_directory, &index)?;
            let new_permission = match value {
                Value::Object(map) if map.is_empty() => None,
                Value::Array(items) if items.is_empty() => None,
                other => Some(other.clone()),
            };

            if source == Some("md") {
                let source_path = source_path.unwrap_or_default();
                let same_target = Some(&source_path) == Some(&target_path);
                if md_data.is_some() && same_target {
                    if let Some(md) = md_data.as_mut() {
                        apply_agent_permission(&mut md.frontmatter, new_permission);
                        md_modified = true;
                    }
                } else {
                    let mut existing = parse_md_file(&source_path)?;
                    apply_agent_permission(&mut existing.frontmatter, new_permission);
                    write_md_file(&source_path, &existing.frontmatter, &existing.body)?;
                    tracing::info!("Updated permission in .md file: {}", source_path.display());
                }
            } else if source == Some("json") {
                let source_path = source_path.unwrap_or_default();
                if Some(&source_path) == Some(&json_target_path) {
                    let entry = entry_map_mut(&mut config, "agent", agent_name)?;
                    apply_agent_permission(entry, new_permission);
                    json_modified = true;
                } else {
                    let mut existing = read_config_file(&source_path)?;
                    let entry = entry_map_mut(&mut existing, "agent", agent_name)?;
                    apply_agent_permission(entry, new_permission);
                    write_config(&existing, &source_path)?;
                    tracing::info!("Updated permission in JSON: {}", source_path.display());
                }
            } else if md_exists && md_data.is_some() {
                if let Some(md) = md_data.as_mut() {
                    apply_agent_permission(&mut md.frontmatter, new_permission);
                    md_modified = true;
                }
            } else if has_json_fields {
                let entry = entry_map_mut(&mut config, "agent", agent_name)?;
                apply_agent_permission(entry, new_permission);
                json_modified = true;
            } else {
                let (write_kind, write_path) = get_json_write_target(&layers, false)?;
                let mut write_config_map = layers.layer(write_kind).clone();
                let entry = entry_map_mut(&mut write_config_map, "agent", agent_name)?;
                apply_agent_permission(entry, new_permission);
                write_config(&write_config_map, &write_path)?;
                tracing::info!("Created permission in JSON: {}", write_path.display());
            }
            continue;
        }

        let in_md = md_data
            .as_ref()
            .is_some_and(|md| md.frontmatter.contains_key(field));
        let in_json = json_section
            .as_ref()
            .is_some_and(|section| section.get(field).is_some());

        if value.is_null() {
            if let Some(md) = md_data.as_mut()
                && in_md
            {
                md.frontmatter.remove(field);
                md_modified = true;
            }
            if in_json
                && config
                    .get("agent")
                    .and_then(|section| section.get(agent_name))
                    .is_some_and(js_truthy)
            {
                delete_entry_field(&mut config, "agent", agent_name, field);
                json_modified = true;
            }
            continue;
        }

        if in_json {
            entry_map_mut(&mut config, "agent", agent_name)?.insert(field.clone(), value.clone());
            json_modified = true;
        } else if in_md || creating_new_md {
            if let Some(md) = md_data.as_mut() {
                md.frontmatter.insert(field.clone(), value.clone());
                md_modified = true;
            }
        } else if (md_exists || creating_new_md) && md_data.is_some() {
            if let Some(md) = md_data.as_mut() {
                md.frontmatter.insert(field.clone(), value.clone());
                md_modified = true;
            }
        } else {
            entry_map_mut(&mut config, "agent", agent_name)?.insert(field.clone(), value.clone());
            json_modified = true;
        }
    }

    if md_modified && let Some(md) = md_data.as_ref() {
        write_md_file(&target_path, &md.frontmatter, &md.body)?;
    }

    if json_modified {
        write_config(&config, &json_target_path)?;
    }

    tracing::info!(
        "Updated agent: {agent_name} (scope: {target_scope}, md: {md_modified}, json: {json_modified})"
    );
    Ok(())
}

/// `deleteJsonAgentEntry` (via the shared section-field deleter).
fn delete_json_agent_entry(config: &mut Map<String, Value>, agent_name: &str) -> bool {
    delete_json_entry(config, "agent", agent_name)
}

/// `deleteAgent`.
pub(crate) fn delete_agent(
    env: &OpenCodeEnv,
    agent_name: &str,
    working_directory: Option<&Path>,
    scope: Option<&str>,
) -> AgentResult<()> {
    let index = agent_index(env);
    let requested_scope = match scope {
        Some(SCOPE_PROJECT) => Some(SCOPE_PROJECT),
        Some(SCOPE_USER) => Some(SCOPE_USER),
        _ => None,
    };

    if (requested_scope.is_none() || requested_scope == Some(SCOPE_PROJECT))
        && let Some(working_directory) = working_directory
    {
        let project_path = project_agent_path(working_directory, agent_name);
        if project_path.exists() {
            std::fs::remove_file(&project_path)
                .map_err(|error| format!("Failed to delete agent file: {error}"))?;
            tracing::info!(
                "Deleted project-level agent .md file: {}",
                project_path.display()
            );
            return Ok(());
        }
    }

    if requested_scope.is_none() || requested_scope == Some(SCOPE_USER) {
        let user_path = user_agent_path(env, agent_name, &index);
        if user_path.exists() {
            std::fs::remove_file(&user_path)
                .map_err(|error| format!("Failed to delete agent file: {error}"))?;
            tracing::info!("Deleted user-level agent .md file: {}", user_path.display());
            return Ok(());
        }
    }

    let layers = read_config_layers(env, working_directory)?;

    if requested_scope == Some(SCOPE_PROJECT) {
        if let Some(project_path) = layers.project_path.clone() {
            let mut project_config = layers.project_config.clone();
            if delete_json_agent_entry(&mut project_config, agent_name) {
                write_config(&project_config, &project_path)?;
                tracing::info!("Removed project-level agent from opencode.json: {agent_name}");
                return Ok(());
            }
        }
        return Err(format!("Project agent {agent_name} not found"));
    }

    if requested_scope == Some(SCOPE_USER) {
        let user_json_path = layers
            .custom_path
            .clone()
            .or_else(|| Some(layers.user_path.clone()));
        let mut user_config = if layers.custom_path.is_some() {
            layers.custom_config.clone()
        } else {
            layers.user_config.clone()
        };
        if let Some(path) = user_json_path
            && delete_json_agent_entry(&mut user_config, agent_name)
        {
            write_config(&user_config, &path)?;
            tracing::info!("Removed user-level agent from opencode.json: {agent_name}");
            return Ok(());
        }
        return Err(format!("User agent {agent_name} not found"));
    }

    let json_source = get_json_entry_source(&layers, "agent", agent_name)?;
    if json_source.exists
        && let (Some(kind), Some(path)) = (json_source.kind, json_source.path.clone())
    {
        let mut layer_config = layers.layer(kind).clone();
        if delete_json_agent_entry(&mut layer_config, agent_name) {
            write_config(&layer_config, &path)?;
            tracing::info!("Removed agent from opencode.json: {agent_name}");
            return Ok(());
        }
    }

    Err(format!("Agent {agent_name} is built-in or not deletable"))
}
