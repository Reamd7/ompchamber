//! Port of `opencode/mcp.js`: MCP server config entries over the JSONC
//! config layers, with the entry-normalization rules of `buildMcpEntry`.
//!
//! 中文说明：移植 `opencode/mcp.js`：在 JSONC 配置层之上实现 MCP
//! 服务器配置的列表/查询/创建/更新/删除，条目统一经 `buildMcpEntry`
//! 归一化（local/remote 类型字段裁剪、headers/env/oauth 清洗、
//! `enabled` 缺省 true）。写入按 scope 选择目标文件（project 为
//! 工作目录下 `.opencode/opencode.json`，否则用户/自定义层）。

use std::path::PathBuf;

use serde_json::{Map, Value, json};

use super::OpenCodeEnv;
use super::agents::SCOPE_PROJECT;
use super::config_layers::{
    get_json_entry_source, get_json_write_target, is_plain_object, read_config_file,
    read_config_layers, write_config,
};
use super::webutil::js_to_string;

/// 中文：MCP 配置操作的统一错误类型（字符串错误，文案对齐 JS）。
pub(crate) type McpResult<T> = Result<T, String>;

/// `validateMcpName`: `^[a-z0-9][a-z0-9_-]*[a-z0-9]$|^[a-z0-9]$`.
/// 中文：校验 MCP 服务器名：单字符只允许小写字母/数字；多字符要求
/// 首尾为小写字母/数字，中间还允许 `-`/`_`（等价 JS 正则
/// `^[a-z0-9][a-z0-9_-]*[a-z0-9]$|^[a-z0-9]$`）。不合法返回固定文案。
fn validate_mcp_name(name: &str) -> McpResult<()> {
    let valid = if name.len() == 1 {
        name.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    } else {
        let bytes = name.as_bytes();
        let edge = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
        !bytes.is_empty()
            && edge(bytes[0])
            && edge(bytes[bytes.len() - 1])
            && bytes[1..bytes.len() - 1]
                .iter()
                .all(|&b| edge(b) || b == b'-' || b == b'_')
    };
    if !valid {
        return Err(
            "MCP server name must be lowercase alphanumeric with hyphens/underscores".to_string(),
        );
    }
    Ok(())
}

/// `resolveMcpScopeFromPath`.
/// 中文：由条目来源路径推断 scope：来源等于项目路径为 `project`，
/// 其他来源为 `user`，无来源（仅内存合并层）为 null。
fn resolve_mcp_scope(
    project_path: Option<&std::path::Path>,
    source_path: Option<&std::path::Path>,
) -> Value {
    match source_path {
        Some(source) if Some(source) == project_path => json!(SCOPE_PROJECT),
        Some(_) => json!(super::agents::SCOPE_USER),
        None => Value::Null,
    }
}

/// `ensureProjectMcpConfigPath`.
/// 中文：确保项目级配置路径存在：按需创建工作目录下的 `.opencode`
/// 目录，返回其中 `opencode.json` 的路径（创建失败静默，写入时报错）。
fn ensure_project_mcp_config_path(working_directory: &std::path::Path) -> PathBuf {
    let config_dir = working_directory.join(".opencode");
    if !config_dir.exists() {
        let _ = std::fs::create_dir_all(&config_dir);
    }
    config_dir.join("opencode.json")
}

/// `if (!config.mcp || typeof config.mcp !== 'object' || isArray) config.mcp = {}`:
/// a missing or non-object section is replaced with a fresh map.
/// 中文：确保配置中存在对象形态的 `mcp` 小节：缺失或非对象（含数组）
/// 时替换为空对象，返回该小节的可变引用（对应 JS 的
/// `if (!config.mcp || ...) config.mcp = {}`）。
fn ensure_mcp_section(config: &mut Map<String, Value>) -> &mut Map<String, Value> {
    let section = config
        .entry("mcp".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !matches!(section, Value::Object(_)) {
        *section = Value::Object(Map::new());
    }
    match section {
        Value::Object(map) => map,
        _ => unreachable!("replaced above"),
    }
}

/// 中文：从合并后的配置中取出 `mcp` 小节的克隆；不存在或非对象时
/// 返回空映射。
fn merged_mcp_section(layers: &super::config_layers::ConfigLayers) -> Map<String, Value> {
    match layers.merged_config.get("mcp") {
        Some(Value::Object(map)) => map.clone(),
        _ => Map::new(),
    }
}

/// `listMcpConfigs`.
/// 中文：列出所有 MCP 服务器配置：遍历合并层 `mcp` 小节（跳过非
/// 对象条目），逐条经 `buildMcp_entry` 归一化、附上 `name` 与推断的
/// `scope`（条目来源经 `get_json_entry_source` 定位）。
pub(crate) fn list_mcp_configs(
    env: &OpenCodeEnv,
    working_directory: Option<&std::path::Path>,
) -> McpResult<Vec<Value>> {
    let layers = read_config_layers(env, working_directory)?;
    let mut configs = Vec::new();
    for (name, entry) in merged_mcp_section(&layers) {
        if !is_plain_object(&entry) {
            continue;
        }
        let source = get_json_entry_source(&layers, "mcp", &name)?;
        let mut value = json!({ "name": name });
        let built = build_mcp_entry(&entry);
        if let (Value::Object(target), Value::Object(fields)) = (&mut value, built) {
            for (key, field) in fields {
                target.insert(key, field);
            }
        }
        if let Value::Object(target) = &mut value {
            target.insert(
                "scope".to_string(),
                resolve_mcp_scope(layers.project_path.as_deref(), source.path.as_deref()),
            );
        }
        configs.push(value);
    }
    Ok(configs)
}

/// `getMcpConfig`.
/// 中文：查询单个 MCP 服务器：合并层中不存在返回 `Ok(None)`；存在则
/// 返回带 `name`、归一化字段与 `scope` 的对象。
pub(crate) fn get_mcp_config(
    env: &OpenCodeEnv,
    name: &str,
    working_directory: Option<&std::path::Path>,
) -> McpResult<Option<Value>> {
    let layers = read_config_layers(env, working_directory)?;
    let Some(entry) = layers
        .merged_config
        .get("mcp")
        .and_then(|mcp| mcp.get(name))
    else {
        return Ok(None);
    };
    let source = get_json_entry_source(&layers, "mcp", name)?;
    let mut value = json!({ "name": name });
    let built = build_mcp_entry(entry);
    if let (Value::Object(target), Value::Object(fields)) = (&mut value, built) {
        for (key, field) in fields {
            target.insert(key, field);
        }
    }
    if let Value::Object(target) = &mut value {
        target.insert(
            "scope".to_string(),
            resolve_mcp_scope(layers.project_path.as_deref(), source.path.as_deref()),
        );
    }
    Ok(Some(value))
}

/// `createMcpConfig`.
/// 中文：创建 MCP 服务器配置：名称必填且需通过校验、不得与既有条目
/// 重名；`scope == "project"` 时写入工作目录的 `.opencode/opencode.json`
/// （缺工作目录报错），否则写入 JS 定位到的用户/自定义层文件。
/// 请求体剥掉 `name` 后归一化入库。
pub(crate) fn create_mcp_config(
    env: &OpenCodeEnv,
    name: &str,
    mcp_config: &Value,
    working_directory: Option<&std::path::Path>,
    scope: Option<&str>,
) -> McpResult<()> {
    if name.is_empty() {
        return Err("MCP server name is required".to_string());
    }
    validate_mcp_name(name)?;

    let layers = read_config_layers(env, working_directory)?;
    if get_json_entry_source(&layers, "mcp", name)?.exists {
        return Err(format!("MCP server \"{name}\" already exists"));
    }

    let (target_path, mut config) = if scope == Some(SCOPE_PROJECT) {
        let Some(working_directory) = working_directory else {
            return Err("Project scope requires working directory".to_string());
        };
        let target_path = ensure_project_mcp_config_path(working_directory);
        let config = if target_path.exists() {
            read_config_file(&target_path)?
        } else {
            Map::new()
        };
        (target_path, config)
    } else {
        let (kind, path) = get_json_write_target(&layers, false)?;
        let config = layers.layer(kind).clone();
        (path, config)
    };

    // `const { name, ...entryData } = mcpConfig`.
    let entry_data = strip_keys(mcp_config, &["name"]);
    let entry = build_mcp_entry(&Value::Object(
        entry_data.into_iter().collect::<Map<String, Value>>(),
    ));
    ensure_mcp_section(&mut config).insert(name.to_string(), entry);

    write_config(&config, &target_path)?;
    tracing::info!("Created MCP server config: {name}");
    Ok(())
}

/// `updateMcpConfig`.
/// 中文：更新 MCP 服务器配置：条目不存在报错；在条目原始来源文件上
/// 合并更新（剥掉 `name`，浅合并到既有条目）再归一化写回，保持条目
/// 所在层级不变。
pub(crate) fn update_mcp_config(
    env: &OpenCodeEnv,
    name: &str,
    updates: &Value,
    working_directory: Option<&std::path::Path>,
) -> McpResult<()> {
    let layers = read_config_layers(env, working_directory)?;
    let source = get_json_entry_source(&layers, "mcp", name)?;

    if !source.exists {
        return Err(format!("MCP server \"{name}\" not found"));
    }

    let target_path = source.path.clone().unwrap_or_else(|| env.config_file());
    let mut config = if let Some(kind) = source.kind {
        layers.layer(kind).clone()
    } else if target_path.exists() {
        read_config_file(&target_path)?
    } else {
        Map::new()
    };

    let existing = config
        .get("mcp")
        .and_then(|mcp| mcp.get(name).cloned())
        .unwrap_or(Value::Object(Map::new()));
    let update_data = strip_keys(updates, &["name"]);
    let mut merged = existing;
    if let (Value::Object(target), Value::Object(fields)) = (
        &mut merged,
        Value::Object(update_data.into_iter().collect()),
    ) {
        for (key, field) in fields {
            target.insert(key, field);
        }
    }

    ensure_mcp_section(&mut config).insert(name.to_string(), build_mcp_entry(&merged));

    write_config(&config, &target_path)?;
    tracing::info!("Updated MCP server config: {name}");
    Ok(())
}

/// `deleteMcpConfig`.
/// 中文：删除 MCP 服务器配置：定位条目来源文件；目标小节中无此条目
/// 报错；删除后若 `mcp` 小节为空则连同小节一起移除，再写回文件。
pub(crate) fn delete_mcp_config(
    env: &OpenCodeEnv,
    name: &str,
    working_directory: Option<&std::path::Path>,
) -> McpResult<()> {
    let layers = read_config_layers(env, working_directory)?;
    let source = get_json_entry_source(&layers, "mcp", name)?;
    let target_path = source.path.clone().unwrap_or_else(|| env.config_file());
    let mut config = if let Some(kind) = source.kind {
        layers.layer(kind).clone()
    } else if target_path.exists() {
        read_config_file(&target_path)?
    } else {
        Map::new()
    };

    let missing = !matches!(
        config.get("mcp"),
        Some(Value::Object(section)) if section.contains_key(name)
    );
    if missing {
        return Err(format!("MCP server \"{name}\" not found"));
    }

    if let Some(Value::Object(section)) = config.get_mut("mcp") {
        section.remove(name);
    }
    if matches!(config.get("mcp"), Some(Value::Object(section)) if section.is_empty()) {
        config.remove("mcp");
    }

    write_config(&config, &target_path)?;
    tracing::info!("Deleted MCP server config: {name}");
    Ok(())
}

/// 中文：从对象中剥除指定键（如 `name`），返回剩余键值对序列；
/// 非对象输入返回空序列（对应 JS 解构 `const { name, ...rest }`）。
fn strip_keys(value: &Value, keys: &[&str]) -> Vec<(String, Value)> {
    match value {
        Value::Object(map) => map
            .iter()
            .filter(|(key, _)| !keys.contains(&key.as_str()))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
        _ => Vec::new(),
    }
}

/// `buildMcpEntry` — normalize an MCP entry: type-local fields, cleaned
/// headers/env/oauth, `enabled` defaulting to true.
/// 中文：归一化 MCP 条目（`buildMcpEntry`）：剥掉 `name`/`scope`；
/// `type` 非 `remote` 一律归为 `local`（并把 command 数组各元素做
/// JS `String()` 强转、删除 remote 专属字段）；remote 则清洗
/// `url`（trim）、`headers`/`environment`（丢空键/空值并强转字符串）、
/// `oauth`（仅保留四个非空字符串字段）并按规则丢弃 `timeout`；
/// 最后 `enabled` 缺省为 true（显式 false 才为 false）。
pub(crate) fn build_mcp_entry(data: &Value) -> Value {
    let mut entry = if is_plain_object(data) {
        match data {
            Value::Object(map) => map.clone(),
            _ => Map::new(),
        }
    } else {
        Map::new()
    };
    entry.remove("name");
    entry.remove("scope");

    let is_remote = data.get("type").and_then(Value::as_str) == Some("remote");
    entry.insert(
        "type".to_string(),
        json!(if is_remote { "remote" } else { "local" }),
    );

    if !is_remote {
        match data.get("command") {
            Some(Value::Array(items)) if !items.is_empty() => {
                entry.insert(
                    "command".to_string(),
                    Value::Array(items.iter().map(js_to_string).map(Value::String).collect()),
                );
            }
            _ => {
                entry.remove("command");
            }
        }
        entry.remove("url");
        entry.remove("headers");
        entry.remove("oauth");
        entry.remove("timeout");
    } else {
        match data.get("url").and_then(Value::as_str) {
            Some(url) => {
                entry.insert("url".to_string(), json!(url.trim()));
            }
            _ => {
                entry.remove("url");
            }
        }
        entry.remove("command");

        match data.get("headers") {
            Some(Value::Object(headers)) => {
                let mut cleaned = Map::new();
                for (key, value) in headers {
                    if key.is_empty() || value.is_null() {
                        continue;
                    }
                    cleaned.insert(key.clone(), Value::String(js_to_string(value)));
                }
                if cleaned.is_empty() {
                    entry.remove("headers");
                } else {
                    entry.insert("headers".to_string(), Value::Object(cleaned));
                }
            }
            Some(_) => {}
            None => {
                entry.remove("headers");
            }
        }
        match data.get("oauth") {
            Some(Value::Bool(false)) => {
                entry.insert("oauth".to_string(), Value::Bool(false));
            }
            Some(oauth) if is_plain_object(oauth) => {
                let mut cleaned = Map::new();
                for field in ["clientId", "clientSecret", "scope", "redirectUri"] {
                    if let Some(text) = oauth.get(field).and_then(Value::as_str)
                        && !text.trim().is_empty()
                    {
                        cleaned.insert(field.to_string(), json!(text.trim()));
                    }
                }
                if cleaned.is_empty() {
                    entry.remove("oauth");
                } else {
                    entry.insert("oauth".to_string(), Value::Object(cleaned));
                }
            }
            Some(_) => {}
            None => {
                entry.remove("oauth");
            }
        }

        let timeout = data.get("timeout");
        let drop_timeout = match timeout {
            None | Some(Value::Null) => true,
            Some(Value::String(text)) => text.is_empty(),
            _ => false,
        };
        if drop_timeout {
            entry.remove("timeout");
        }
        // Values that are neither the drop cases nor positive finite
        // numbers stay verbatim from the clone, exactly like the JS.
    }

    match data.get("environment") {
        Some(Value::Object(environment)) => {
            let mut cleaned = Map::new();
            for (key, value) in environment {
                if key.is_empty() || value.is_null() {
                    continue;
                }
                cleaned.insert(key.clone(), Value::String(js_to_string(value)));
            }
            if cleaned.is_empty() {
                entry.remove("environment");
            } else {
                entry.insert("environment".to_string(), Value::Object(cleaned));
            }
        }
        Some(_) => {}
        None => {
            entry.remove("environment");
        }
    }

    entry.insert(
        "enabled".to_string(),
        json!(data.get("enabled") != Some(&Value::Bool(false))),
    );

    Value::Object(entry)
}
