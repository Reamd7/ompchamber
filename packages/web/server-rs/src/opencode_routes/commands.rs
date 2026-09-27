//! Port of `opencode/commands.js`: command config CRUD across project/user
//! `.md` files and the JSONC config layers.
//! 中文说明:本模块移植自 `opencode/commands.js`,实现 command(斜杠
//! 命令)配置的增删改查:项目/用户目录下的 `.md` 定义与 JSONC 配置
//! 层中的 `command` 段;路径解析、作用域与写回规则与 agents 模块保持
//! 一致,本模块只做纯逻辑与磁盘读写,HTTP 路由在 entity_routes 中。

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

/// command 模块统一的结果别名:错误为携带用户可读消息的 `String`,
/// 由路由层转换为 HTTP 错误响应。
pub(crate) type CommandResult<T> = Result<T, String>;

/// `ensureProjectCommandDir` — creates both the plural and legacy dirs.
/// 创建项目级 command 的两个目录:新版复数 `.opencode/commands` 与
/// 旧版单数 `.opencode/command`,返回复数目录路径;已存在则跳过,
/// 创建失败静默忽略(错误延迟到写文件时才暴露)。
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
/// 计算项目级 command 的 `.md` 路径:默认复数形式
/// `.opencode/commands/<name>.md`,仅当旧版单数路径存在且复数路径
/// 不存在时回退到旧版。
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
/// 计算用户级 command 的 `.md` 路径:默认 command 目录下的复数形式,
/// 仅当旧版 `config/command/<name>.md` 存在且新路径不存在时回退。
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
/// 判定 command 当前归属的作用域:先查项目级 `.md`(命中返回
/// project),再查用户级 `.md`(返回 user);都不存在返回
/// `(None, None)`。返回元组为 (作用域标签, 文件路径)。
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
/// 计算 command 更新时的写入路径:已有定义沿用其作用域与路径;
/// 否则显式请求 project 作用域且有工作目录时落到项目路径,默认
/// 用户级路径。
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
/// 汇总 command 在 Markdown 与 JSON 两类存储中的来源信息,返回
/// `md`/`json`/`projectMd`/`userMd` 四组(各含 exists/path/scope/
/// fields;`.md` 非空正文以 `template` 字段名出现)。JSON 来源经
/// `get_json_entry_source` 解析,无条目时 path 依次回退
/// custom → project → user 层;配置层读取或 `.md` 解析失败向上返回 Err。
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
/// 新建 command:先确保目录存在,再检查项目 `.md`、用户 `.md` 与
/// JSON 层三处均无同名定义(重复即报错);随后按请求的 scope 选择
/// 写入路径(未指定或无工作目录时默认用户级,project 作用域会先
/// 创建项目目录)。写入内容:剥离 `template`/`scope` 键后的字段作为
/// frontmatter,正文取 `template` 字符串;成功后记录日志。
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
/// 按 JS `updateCommand` 语义逐字段更新 command。`.md` 与 JSON 均无
/// 定义时视为内建覆盖(is_builtin_override),新建用户级 `.md`;JSON
/// 写入目标在无既有条目但有项目目录时优先 project 层。字段路由:
/// `template` 写 `.md` 正文或 JSON 条目(JSON 侧 template 为文件引用
/// 时改写引用文件);其它字段按当前所在存储就地更新(in_json →
/// JSON;in_md 或新建 → `.md`;都没有 → JSON)。循环结束后按
/// md_modified/json_modified 标志分别落盘并记录日志。
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
/// 删除 command:依次删除项目 `.md`、用户 `.md`,再从条目所在 JSON
/// 层删除条目(三步独立执行,能删尽删,任一成功即计入);全部未
/// 命中返回 Err("not found");文件删除或配置写回失败同样返回 Err。
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
