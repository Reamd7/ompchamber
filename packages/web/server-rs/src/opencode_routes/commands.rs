//! Port of `opencode/commands.js`: command config CRUD across project/user
//! `.md` files and the JSONC config layers.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use super::OpenCodeEnv;
use super::agents::SCOPE_PROJECT;
use super::config_layers::{
    LayerKind, delete_json_entry, entry_map_mut, get_json_entry_source, get_json_write_target,
    read_config_layers, write_config,
};
use super::md_file::{
    MdFile, ensure_dirs, is_prompt_file_reference, parse_md_file, resolve_prompt_file_path,
    write_md_file, write_prompt_file,
};
use super::webutil::{js_to_string, object_keys};

pub(crate) type CommandResult<T> = Result<T, String>;

/// `ensureProjectCommandDir` — creates both the plural and legacy dirs.
fn ensure_project_command_dir(working_directory: &Path) -> PathBuf {
    let dir = working_directory.join(".opencode").join("commands");
    if !dir.exists() {
        let _ = std::fs::create_dir_all(&dir);
    }
    let legacy = working_directory.join(".opencode").join("command");
    if !legacy.exists() {
        let _ = std::fs::create_dir_all(&legacy);
    }
    dir
}

/// `getProjectCommandPath`.
fn project_command_path(working_directory: &Path, command_name: &str) -> PathBuf {
    let plural = working_directory
        .join(".opencode")
        .join("commands")
        .join(format!("{command_name}.md"));
    let legacy = working_directory
        .join(".opencode")
        .join("command")
        .join(format!("{command_name}.md"));
    if legacy.exists() && !plural.exists() {
        return legacy;
    }
    plural
}

/// `getUserCommandPath`.
fn user_command_path(env: &OpenCodeEnv, command_name: &str) -> PathBuf {
    let plural = env.command_dir().join(format!("{command_name}.md"));
    let legacy = env
        .config_dir
        .join("command")
        .join(format!("{command_name}.md"));
    if legacy.exists() && !plural.exists() {
        return legacy;
    }
    plural
}

/// `getCommandScope`.
fn command_scope(
    env: &OpenCodeEnv,
    command_name: &str,
    working_directory: Option<&Path>,
) -> (Option<&'static str>, Option<PathBuf>) {
    if let Some(working_directory) = working_directory {
        let project_path = project_command_path(working_directory, command_name);
        if project_path.exists() {
            return (Some(SCOPE_PROJECT), Some(project_path));
        }
    }
    let user_path = user_command_path(env, command_name);
    if user_path.exists() {
        return (Some(super::agents::SCOPE_USER), Some(user_path));
    }
    (None, None)
}

/// `getCommandWritePath`.
fn command_write_path(
    env: &OpenCodeEnv,
    command_name: &str,
    working_directory: Option<&Path>,
    requested_scope: Option<&str>,
) -> (&'static str, PathBuf) {
    if let (Some(scope), Some(path)) = command_scope(env, command_name, working_directory) {
        return (scope, path);
    }
    if requested_scope == Some(SCOPE_PROJECT)
        && let Some(working_directory) = working_directory
    {
        return (
            SCOPE_PROJECT,
            project_command_path(working_directory, command_name),
        );
    }
    (
        super::agents::SCOPE_USER,
        user_command_path(env, command_name),
    )
}

/// `getCommandSources`.
pub(crate) fn get_command_sources(
    env: &OpenCodeEnv,
    command_name: &str,
    working_directory: Option<&Path>,
) -> CommandResult<Value> {
    let project_path = working_directory.map(|wd| project_command_path(wd, command_name));
    let project_exists = project_path.as_ref().is_some_and(|p| p.exists());

    let user_path = user_command_path(env, command_name);
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
        json!(super::agents::SCOPE_USER)
    } else {
        Value::Null
    };

    let layers = read_config_layers(env, working_directory)?;
    let json_source = get_json_entry_source(&layers, "command", command_name)?;
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
        json!(super::agents::SCOPE_USER)
    };

    let mut md_fields: Vec<Value> = Vec::new();
    if let Some(md_path) = md_path.as_deref() {
        let md = parse_md_file(md_path)?;
        md_fields = object_keys(&Value::Object(md.frontmatter))
            .into_iter()
            .map(Value::String)
            .collect();
        if !md.body.is_empty() {
            md_fields.push(Value::String("template".to_string()));
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
            "path": md_path.as_ref().map(|p| json!(p.to_string_lossy())).unwrap_or(Value::Null),
            "scope": md_scope,
            "fields": md_fields,
        },
        "json": {
            "exists": json_source.exists,
            "path": json_path.as_ref().map(|p| json!(p.to_string_lossy())).unwrap_or(Value::Null),
            "scope": if json_source.exists { json_scope } else { Value::Null },
            "fields": json_fields,
        },
        "projectMd": {
            "exists": project_exists,
            "path": project_path.as_ref().map(|p| json!(p.to_string_lossy())).unwrap_or(Value::Null),
        },
        "userMd": {
            "exists": user_exists,
            "path": json!(user_path.to_string_lossy()),
        },
    }))
}

/// `createCommand`.
pub(crate) fn create_command(
    env: &OpenCodeEnv,
    command_name: &str,
    config: &Map<String, Value>,
    working_directory: Option<&Path>,
    scope: Option<&str>,
) -> CommandResult<()> {
    ensure_dirs(env);

    let project_path = working_directory.map(|wd| project_command_path(wd, command_name));
    let user_path = user_command_path(env, command_name);

    if let Some(project_path) = project_path.as_ref()
        && project_path.exists()
    {
        return Err(format!(
            "Command {command_name} already exists as project-level .md file"
        ));
    }
    if user_path.exists() {
        return Err(format!(
            "Command {command_name} already exists as user-level .md file"
        ));
    }

    let layers = read_config_layers(env, working_directory)?;
    if get_json_entry_source(&layers, "command", command_name)?.exists {
        return Err(format!(
            "Command {command_name} already exists in opencode.json"
        ));
    }

    let (target_path, target_scope) = if scope == Some(SCOPE_PROJECT)
        && let Some(working_directory) = working_directory
    {
        ensure_project_command_dir(working_directory);
        (
            project_command_path(working_directory, command_name),
            SCOPE_PROJECT,
        )
    } else {
        (user_path, super::agents::SCOPE_USER)
    };

    // `const { template, scope, ...frontmatter } = config`.
    let mut frontmatter: Map<String, Value> = Map::new();
    for (key, value) in config {
        if key == "template" || key == "scope" {
            continue;
        }
        frontmatter.insert(key.clone(), value.clone());
    }
    let template = config
        .get("template")
        .and_then(Value::as_str)
        .unwrap_or_default();

    write_md_file(&target_path, &frontmatter, template)?;
    tracing::info!(
        "Created new command: {command_name} (scope: {target_scope}, path: {})",
        target_path.display()
    );
    Ok(())
}

/// `updateCommand`.
pub(crate) fn update_command(
    env: &OpenCodeEnv,
    command_name: &str,
    updates: &Map<String, Value>,
    working_directory: Option<&Path>,
) -> CommandResult<()> {
    ensure_dirs(env);

    let (scope, md_path) = command_write_path(env, command_name, working_directory, None);
    let md_exists = md_path.exists();

    let layers = read_config_layers(env, working_directory)?;
    let json_source = get_json_entry_source(&layers, "command", command_name)?;
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
        let prefer_project = working_directory.is_some();
        get_json_write_target(&layers, prefer_project)?
    };
    let mut config = layers.layer(target_kind).clone();

    let is_builtin_override = !md_exists && !has_json_fields;

    let mut target_path = md_path.clone();
    let mut target_scope = scope.to_string();
    if !md_exists && is_builtin_override {
        target_path = user_command_path(env, command_name);
        target_scope = super::agents::SCOPE_USER.to_string();
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
        if field == "template" {
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
            let section_template = json_section
                .as_ref()
                .and_then(|section| section.get("template").cloned())
                .unwrap_or(Value::Null);
            if is_prompt_file_reference(&section_template) {
                let template_path =
                    resolve_prompt_file_path(env, &section_template).ok_or_else(|| {
                        format!("Invalid template file reference for command {command_name}")
                    })?;
                write_prompt_file(&template_path, &normalized);
                continue;
            } else if is_prompt_file_reference(&Value::String(normalized.clone())) {
                entry_map_mut(&mut config, "command", command_name)?
                    .insert("template".to_string(), Value::String(normalized));
                json_modified = true;
                continue;
            }

            entry_map_mut(&mut config, "command", command_name)?
                .insert("template".to_string(), Value::String(normalized));
            json_modified = true;
            continue;
        }

        let in_md = md_data
            .as_ref()
            .is_some_and(|md| md.frontmatter.contains_key(field));
        let in_json = json_section
            .as_ref()
            .is_some_and(|section| section.get(field).is_some());

        if in_json {
            entry_map_mut(&mut config, "command", command_name)?
                .insert(field.clone(), value.clone());
            json_modified = true;
        } else if in_md || creating_new_md {
            if let Some(md) = md_data.as_mut() {
                md.frontmatter.insert(field.clone(), value.clone());
                md_modified = true;
            }
        } else {
            if (md_exists || creating_new_md) && md_data.is_some() {
                if let Some(md) = md_data.as_mut() {
                    md.frontmatter.insert(field.clone(), value.clone());
                    md_modified = true;
                }
            } else {
                entry_map_mut(&mut config, "command", command_name)?
                    .insert(field.clone(), value.clone());
                json_modified = true;
            }
        }
    }

    if md_modified && let Some(md) = md_data.as_ref() {
        write_md_file(&target_path, &md.frontmatter, &md.body)?;
    }

    if json_modified {
        write_config(&config, &json_target_path)?;
    }

    tracing::info!(
        "Updated command: {command_name} (scope: {target_scope}, md: {md_modified}, json: {json_modified})"
    );
    Ok(())
}

/// `deleteCommand`.
pub(crate) fn delete_command(
    env: &OpenCodeEnv,
    command_name: &str,
    working_directory: Option<&Path>,
) -> CommandResult<()> {
    let mut deleted = false;

    if let Some(working_directory) = working_directory {
        let project_path = project_command_path(working_directory, command_name);
        if project_path.exists() {
            std::fs::remove_file(&project_path)
                .map_err(|error| format!("Failed to delete command file: {error}"))?;
            tracing::info!(
                "Deleted project-level command .md file: {}",
                project_path.display()
            );
            deleted = true;
        }
    }

    let user_path = user_command_path(env, command_name);
    if user_path.exists() {
        std::fs::remove_file(&user_path)
            .map_err(|error| format!("Failed to delete command file: {error}"))?;
        tracing::info!(
            "Deleted user-level command .md file: {}",
            user_path.display()
        );
        deleted = true;
    }

    let layers = read_config_layers(env, working_directory)?;
    let json_source = get_json_entry_source(&layers, "command", command_name)?;
    if json_source.exists
        && let (Some(kind), Some(path)) = (json_source.kind, json_source.path.clone())
    {
        let mut layer_config = layers.layer(kind).clone();
        if delete_json_entry(&mut layer_config, "command", command_name) {
            write_config(&layer_config, &path)?;
            tracing::info!("Removed command from opencode.json: {command_name}");
            deleted = true;
        }
    }

    if !deleted {
        return Err(format!("Command \"{command_name}\" not found"));
    }
    Ok(())
}
