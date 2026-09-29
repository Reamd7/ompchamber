//! Port of `server/lib/opencode/plugins.js` — the plugin config data layer.
//!
//! Plugin entries live in the `plugin` array of OpenCode JSONC configs
//! (custom `OPENCODE_CONFIG` file, else the user config, plus the project
//! config). Entries are either plain spec strings or `[spec, options]`
//! tuples. Plugin files are loose `.js/.ts/.mjs/.cjs` files in the
//! `plugins/` directory beside the config (project scope: `<project>/
//! .opencode/plugins`).
//!
//! Ids are base64url of `config:scope:spec` (entries) and
//! `file:scope:fileName` (files); Node's lenient base64url decoding is
//! mirrored by ignoring non-alphabet characters.
//!
//! 中文说明：本模块是 OpenCode 插件配置数据层（plugins.js 的 Rust 移植），
//! 管理两类资源：配置文件 `plugin` 数组里的插件条目（纯字符串 spec 或
//! `[spec, options]` 元组），以及配置目录旁 `plugins/` 目录下散落的
//! `.js/.ts/.mjs/.cjs` 插件文件。所有错误以 CodedError（message + 稳定
//! 错误码）返回，供 HTTP 层映射状态码与响应文案。

use std::path::{Path, PathBuf};

use base64::Engine;
use serde_json::{Map, Value};

use super::config_layers::{
    CodedError, active_custom_config_path, active_opencode_config_dir, primary_user_config_path,
    project_config_path, read_config_file, read_config_layer, write_config,
};
use super::plugin_spec::is_path_spec;

/// 错误码：要创建的插件条目在该 scope 下已存在（ENTRY_EXISTS）。
const CODE_ENTRY_EXISTS: &str = "ENTRY_EXISTS";
/// 错误码：要写入的插件文件已存在且未允许覆盖（FILE_EXISTS）。
const CODE_FILE_EXISTS: &str = "FILE_EXISTS";
/// 错误码：按 id 查找的插件条目/文件不存在（NOT_FOUND）。
const CODE_NOT_FOUND: &str = "NOT_FOUND";
/// 错误码：插件文件名非法（格式不符或含路径穿越）（INVALID_FILENAME）。
const CODE_INVALID_FILENAME: &str = "INVALID_FILENAME";
/// 错误码：scope 不是 user/project，或 project scope 缺少工作目录（INVALID_SCOPE）。
const CODE_INVALID_SCOPE: &str = "INVALID_SCOPE";
/// 错误码：插件 spec 非法（非字符串、空白、含 NUL 或 id 形状错误）（INVALID_SPEC）。
const CODE_INVALID_SPEC: &str = "INVALID_SPEC";

/// scope 常量：用户级（OPENCODE_CONFIG 目录所属的配置层）。
pub(crate) const SCOPE_USER: &str = "user";
/// scope 常量：项目级（工作目录下 `.opencode/` 内的配置与插件目录）。
pub(crate) const SCOPE_PROJECT: &str = "project";

/// 以指定错误码构造 CodedError 的便捷包装。
fn coded(message: impl Into<String>, code: &'static str) -> CodedError {
    CodedError::new(message, code)
}

/// 校验 scope 必须是 "user" 或 "project"，否则返回 INVALID_SCOPE 错误。
fn validate_scope(scope: &str) -> Result<(), CodedError> {
    if scope != SCOPE_USER && scope != SCOPE_PROJECT {
        return Err(coded(
            "Plugin scope must be user or project",
            CODE_INVALID_SCOPE,
        ));
    }
    Ok(())
}

/// 校验插件 spec：必须是 trim 后非空且不含 NUL 字节的字符串，返回 trim 后的值；非法时返回 INVALID_SPEC。
fn validate_plugin_spec(spec: &Value) -> Result<String, CodedError> {
    let Some(text) = spec.as_str() else {
        return Err(coded(
            "Plugin spec must be a non-empty string",
            CODE_INVALID_SPEC,
        ));
    };
    if text.trim().is_empty() {
        return Err(coded(
            "Plugin spec must be a non-empty string",
            CODE_INVALID_SPEC,
        ));
    }
    if text.contains('\0') {
        return Err(coded(
            "Plugin spec cannot contain null bytes",
            CODE_INVALID_SPEC,
        ));
    }
    Ok(text.trim().to_string())
}

/// 判断 JSON 值是否为对象（object）。
fn is_record(value: &Value) -> bool {
    matches!(value, Value::Object(_))
}

/// 判断 options 是否为有效选项：存在、是对象且非空；决定序列化时是否写成 [spec, options] 元组。
fn has_options(options: Option<&Value>) -> bool {
    options.is_some_and(|options| {
        is_record(options) && !options.as_object().is_some_and(|m| m.is_empty())
    })
}

/// 依据 spec 判定插件类别：路径型 spec（见 plugin_spec::is_path_spec）返回 "path"，否则返回 "npm"。
pub(crate) fn parsed_kind_for_spec(spec: &str) -> &'static str {
    if is_path_spec(spec) { "path" } else { "npm" }
}

/// `encodePluginId` — base64url (no padding) of `prefix:value`.
/// 中文注解：把 prefix:value 拼接后做无填充 base64url 编码，生成插件条目/文件的稳定 id。
pub(crate) fn encode_plugin_id(prefix: &str, value: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!("{prefix}:{value}"))
}

/// `decodePluginId` — Node-lenient base64url decode, then split at the first
/// colon.
/// 中文注解：先按 Node 宽容策略滤掉非 base64url 字符再解码（失败得到空串），
/// 随后在解码结果的第一个冒号处切分成 (prefix, value)；无冒号报 INVALID_SPEC。
pub(crate) fn decode_plugin_id(id: &str) -> Result<(String, String), CodedError> {
    let filtered: String = id
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '-' || *ch == '_')
        .collect();
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(filtered.as_bytes())
        .unwrap_or_default();
    let decoded = String::from_utf8_lossy(&decoded).into_owned();
    let Some(separator) = decoded.find(':') else {
        return Err(coded("Invalid plugin id", CODE_INVALID_SPEC));
    };
    Ok((
        decoded[..separator].to_string(),
        decoded[separator + 1..].to_string(),
    ))
}

/// `parsePluginRaw` — string or `[string, object]` tuple.
/// 中文注解：接受纯字符串 spec 或 [spec字符串, options对象] 二元组（空对象也保留为 Some），
/// 其余形状报 INVALID_SPEC；spec 顺带经过合法性校验。
pub(crate) fn parse_plugin_raw(raw: &Value) -> Result<ParsedPlugin, CodedError> {
    if let Some(text) = raw.as_str() {
        return Ok(ParsedPlugin {
            spec: validate_plugin_spec(&Value::from(text))?,
            options: None,
        });
    }
    if let Some(items) = raw.as_array()
        && items.len() == 2
        && is_record(&items[1])
    {
        return Ok(ParsedPlugin {
            spec: validate_plugin_spec(&items[0])?,
            options: Some(items[1].clone()),
        });
    }
    Err(coded(
        "Plugin spec must be a string or [string, object]",
        CODE_INVALID_SPEC,
    ))
}

/// 从配置数组项解析出的插件：spec 与可选 options。
#[derive(Debug)]
pub(crate) struct ParsedPlugin {
    /// 插件标识（npm 包名或文件路径），已 trim。
    pub spec: String,
    /// 插件选项对象；纯字符串条目时为 None。
    pub options: Option<Value>,
}

/// `serializePluginEntry` — tuple only when options is a non-empty object.
/// 中文注解：options 为非空对象时序列化为 [spec, options] 元组，否则序列化为纯字符串。
pub(crate) fn serialize_plugin_entry(spec: &str, options: Option<&Value>) -> Value {
    if has_options(options) {
        return Value::Array(vec![
            Value::from(spec),
            options.cloned().unwrap_or(Value::Null),
        ]);
    }
    Value::from(spec)
}

/// 一次读取得到的三层插件配置（custom / user / project）内容及各自文件路径。
struct ConfigLayers {
    /// 用户层配置内容。
    user_config: Map<String, Value>,
    /// 项目层配置内容。
    project_config: Map<String, Value>,
    /// OPENCODE_CONFIG 自定义层配置内容。
    custom_config: Map<String, Value>,
    /// 用户层配置文件路径（总是存在）。
    user_path: PathBuf,
    /// 项目层配置文件路径（无工作目录时为 None）。
    project_path: Option<PathBuf>,
    /// OPENCODE_CONFIG 指定的配置文件路径（未设置该环境变量时为 None）。
    custom_path: Option<PathBuf>,
}

/// 解析三层配置路径并逐一读取（read_config_layer 对 INVALID_JSONC 降级为空配置），汇总成 ConfigLayers。
fn read_plugin_config_layers(working_directory: Option<&Path>) -> Result<ConfigLayers, CodedError> {
    let custom_path = active_custom_config_path();
    let user_path = primary_user_config_path();
    let project_path = project_config_path(working_directory);
    let user_layer = read_config_layer(Some(&user_path))?;
    let project_layer = read_config_layer(project_path.as_deref())?;
    let custom_layer = read_config_layer(custom_path.as_deref())?;
    Ok(ConfigLayers {
        user_config: user_layer.config,
        project_config: project_layer.config,
        custom_config: custom_layer.config,
        user_path,
        project_path,
        custom_path,
    })
}

/// 参与插件枚举/写入的一个配置来源：配置内容、文件路径与所属 scope。
struct ConfigSource {
    /// 该来源的完整配置对象。
    config: Map<String, Value>,
    /// 该来源对应的配置文件路径（也是写入目标）。
    file_path: PathBuf,
    /// 该来源的 scope（user 或 project）。
    scope: &'static str,
}

/// 把三层配置展开成有序来源列表：设置了 OPENCODE_CONFIG 时 custom 层顶替 user 层（同为 user scope），
/// 其后追加 project 层；列表顺序即枚举顺序。
fn config_sources(layers: &ConfigLayers) -> Vec<ConfigSource> {
    let mut sources = Vec::new();
    if let Some(custom_path) = &layers.custom_path {
        sources.push(ConfigSource {
            config: layers.custom_config.clone(),
            file_path: custom_path.clone(),
            scope: SCOPE_USER,
        });
    } else {
        sources.push(ConfigSource {
            config: layers.user_config.clone(),
            file_path: layers.user_path.clone(),
            scope: SCOPE_USER,
        });
    }
    if let Some(project_path) = &layers.project_path {
        sources.push(ConfigSource {
            config: layers.project_config.clone(),
            file_path: project_path.clone(),
            scope: SCOPE_PROJECT,
        });
    }
    sources
}

/// 在第一个冒号处把 "scope:spec" 形式的值切成两段；缺少冒号报 INVALID_SPEC。
fn split_scoped_value(value: &str) -> Result<(&str, &str), CodedError> {
    let Some(separator) = value.find(':') else {
        return Err(coded(
            "Plugin id value must include scope",
            CODE_INVALID_SPEC,
        ));
    };
    Ok((&value[..separator], &value[separator + 1..]))
}

/// A located plugin entry: the owning config source plus the raw array index.
/// 中文注解：已定位的插件条目，用所属来源下标 + plugin 数组原始下标双重定位。
struct PluginTarget {
    /// 所属配置来源在 config_sources() 返回列表中的下标。
    source_index: usize,
    /// 条目在来源配置 plugin 数组中的下标。
    plugin_index: usize,
}

/// 解码 id 并在对应 scope 的配置层中按 spec 查找条目位置；prefix 不是 "config" 报 INVALID_SPEC，
/// 未找到返回 Ok(None)，配置读取错误原样上抛。
fn find_plugin_target(
    id: &str,
    working_directory: Option<&Path>,
) -> Result<Option<PluginTarget>, CodedError> {
    let (prefix, value) = decode_plugin_id(id)?;
    if prefix != "config" {
        return Err(coded(
            "Plugin entry id must use config prefix",
            CODE_INVALID_SPEC,
        ));
    }
    let (scope, spec) = split_scoped_value(&value)?;
    validate_scope(scope)?;
    let layers = read_plugin_config_layers(working_directory)?;
    let sources = config_sources(&layers);
    for (source_index, source) in sources.iter().enumerate() {
        if source.scope != scope {
            continue;
        }
        let plugin = source.config.get("plugin").and_then(Value::as_array);
        let Some(plugin) = plugin else {
            return Ok(None);
        };
        for (plugin_index, raw) in plugin.iter().enumerate() {
            if parse_plugin_raw(raw)?.spec == spec {
                return Ok(Some(PluginTarget {
                    source_index,
                    plugin_index,
                }));
            }
        }
        return Ok(None);
    }
    Ok(None)
}

/// 解析某 scope 的插件文件目录：project 为 <工作目录>/.opencode/plugins（缺工作目录报 INVALID_SCOPE），
/// user 为当前 OPENCODE_CONFIG 目录下的 plugins。
fn plugin_dir_for_scope(
    scope: &str,
    working_directory: Option<&Path>,
) -> Result<PathBuf, CodedError> {
    validate_scope(scope)?;
    if scope == SCOPE_PROJECT {
        let Some(working_directory) = working_directory else {
            return Err(coded(
                "Project scope requires working directory",
                CODE_INVALID_SCOPE,
            ));
        };
        return Ok(working_directory.join(".opencode").join("plugins"));
    }
    Ok(active_opencode_config_dir().join("plugins"))
}

/// 由 "file:" 前缀 id 解析出的插件文件目标：文件名、scope 与绝对路径。
struct FileTarget {
    /// 已通过校验的插件文件名。
    file_name: String,
    /// 文件所属 scope（user 或 project）。
    scope: String,
    /// 插件目录下该文件的绝对路径。
    absolute_path: PathBuf,
}

/// 解码并校验 "file:scope:fileName" 形式的 id（prefix、scope、文件名逐项校验），
/// 再拼出插件目录下的绝对路径；prefix 不符报 INVALID_FILENAME。
fn file_target_from_id(
    id: &str,
    working_directory: Option<&Path>,
) -> Result<FileTarget, CodedError> {
    let (prefix, value) = decode_plugin_id(id)?;
    if prefix != "file" {
        return Err(coded(
            "Plugin file id must use file prefix",
            CODE_INVALID_FILENAME,
        ));
    }
    let (scope, file_name) = split_scoped_value(&value)?;
    validate_scope(scope)?;
    let file_name = validate_file_name(file_name)?;
    Ok(FileTarget {
        absolute_path: plugin_dir_for_scope(scope, working_directory)?.join(&file_name),
        file_name,
        scope: scope.to_string(),
    })
}

/// `/^[a-z0-9][a-z0-9-_.]*\.(js|ts|mjs|cjs)$/` plus traversal rejection.
/// 中文注解：先拒绝路径穿越（/、反斜杠、..），再要求匹配
/// ^[a-z0-9][a-z0-9-_.]*\.(js|ts|mjs|cjs)$；通过则原样返回文件名。
fn validate_file_name(file_name: &str) -> Result<String, CodedError> {
    if file_name.is_empty() {
        return Err(coded("Plugin file name is required", CODE_INVALID_FILENAME));
    }
    if file_name.contains('/')
        || file_name.contains('\\')
        || file_name.contains("..")
        || !plugin_file_name_ok(file_name)
    {
        return Err(coded(
            "Plugin file name must match /^[a-z0-9][a-z0-9-_.]*\\.(js|ts|mjs|cjs)$/ and cannot contain path traversal",
            CODE_INVALID_FILENAME,
        ));
    }
    Ok(file_name.to_string())
}

/// 纯字符级实现插件文件名正则：扩展名限 js/ts/mjs/cjs，首字符为小写字母或数字，
/// 其余字符限小写字母/数字/-/_/.。
fn plugin_file_name_ok(file_name: &str) -> bool {
    let bytes = file_name.as_bytes();
    let Some(dot) = file_name.rfind('.') else {
        return false;
    };
    let stem = &file_name[..dot];
    let extension = &file_name[dot + 1..];
    if !matches!(extension, "js" | "ts" | "mjs" | "cjs") {
        return false;
    }
    if stem.is_empty() {
        return false;
    }
    let first = bytes[0];
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return false;
    }
    stem.bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.'))
}

/// `listPluginEntries` — user layer first, then project.
/// 中文注解：按 user（或 custom）层在前、project 层在后的顺序枚举全部插件条目，
/// 输出带 id/spec/options/scope/kind/parsedKind/sourcePath 的 JSON 对象数组。
pub(crate) fn list_plugin_entries(
    working_directory: Option<&Path>,
) -> Result<Vec<Value>, CodedError> {
    let layers = read_plugin_config_layers(working_directory)?;
    let mut entries = Vec::new();
    for source in config_sources(&layers) {
        let Some(plugin) = source.config.get("plugin").and_then(Value::as_array) else {
            continue;
        };
        for raw in plugin {
            let parsed = parse_plugin_raw(raw)?;
            let mut entry = Map::new();
            entry.insert(
                "id".into(),
                Value::from(encode_plugin_id(
                    "config",
                    &format!("{}:{}", source.scope, parsed.spec),
                )),
            );
            entry.insert("spec".into(), Value::from(parsed.spec.clone()));
            if let Some(options) = &parsed.options {
                entry.insert("options".into(), options.clone());
            }
            entry.insert("scope".into(), Value::from(source.scope));
            entry.insert("kind".into(), Value::from("config"));
            entry.insert(
                "parsedKind".into(),
                Value::from(parsed_kind_for_spec(&parsed.spec)),
            );
            entry.insert(
                "sourcePath".into(),
                Value::from(source.file_path.to_string_lossy().as_ref()),
            );
            entries.push(Value::Object(entry));
        }
    }
    Ok(entries)
}

/// 按完整 id 在列表结果中查找单个插件条目；找不到返回 Ok(None)。
pub(crate) fn get_plugin_entry(
    id: &str,
    working_directory: Option<&Path>,
) -> Result<Option<Value>, CodedError> {
    let entries = list_plugin_entries(working_directory)?;
    Ok(entries
        .into_iter()
        .find(|entry| entry.get("id").and_then(Value::as_str) == Some(id)))
}

/// 确保项目配置文件路径可用：无工作目录报 INVALID_SCOPE，必要时创建 <工作目录>/.opencode
/// 目录（失败报 IO），返回其中 opencode.json 的路径。
fn ensure_project_config_path(working_directory: Option<&Path>) -> Result<PathBuf, CodedError> {
    let Some(working_directory) = working_directory else {
        return Err(coded(
            "Project scope requires working directory",
            CODE_INVALID_SCOPE,
        ));
    };
    let config_dir = working_directory.join(".opencode");
    std::fs::create_dir_all(&config_dir)
        .map_err(|_| coded("Failed to create project config directory", "IO"))?;
    Ok(config_dir.join("opencode.json"))
}

/// 新增插件条目：校验 spec 与 scope；同 scope 内重复 spec 报 ENTRY_EXISTS；
/// project scope 写项目配置（必要时新建），user scope 优先写 custom 配置层；
/// 追加序列化条目后经 write_config 落盘。
pub(crate) fn create_plugin_entry(
    spec: &Value,
    options: Option<&Value>,
    scope: Option<&str>,
    working_directory: Option<&Path>,
) -> Result<(), CodedError> {
    let spec = validate_plugin_spec(spec)?;
    let scope = scope.unwrap_or(SCOPE_USER);
    let layers = read_plugin_config_layers(working_directory)?;
    let existing = config_sources(&layers).into_iter().any(|source| {
        source.scope == scope
            && source
                .config
                .get("plugin")
                .and_then(Value::as_array)
                .is_some_and(|plugin| {
                    plugin
                        .iter()
                        .any(|raw| parse_plugin_raw(raw).is_ok_and(|p| p.spec == spec))
                })
    });
    if existing {
        return Err(coded(
            format!("Plugin \"{spec}\" already exists"),
            CODE_ENTRY_EXISTS,
        ));
    }

    let (target_path, mut config) = if scope == SCOPE_PROJECT {
        let target_path = ensure_project_config_path(working_directory)?;
        let config = if target_path.exists() {
            read_config_file(&target_path)?
        } else {
            Map::new()
        };
        (target_path, config)
    } else {
        match &layers.custom_path {
            Some(custom_path) => (custom_path.clone(), layers.custom_config.clone()),
            None => (layers.user_path.clone(), layers.user_config.clone()),
        }
    };

    let plugin = config
        .entry("plugin".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    if !plugin.is_array() {
        *plugin = Value::Array(Vec::new());
    }
    if let Some(items) = plugin.as_array_mut() {
        items.push(serialize_plugin_entry(&spec, options));
    }
    write_config(&config, &target_path)
}

/// 按 id 原地更新插件条目：仅覆盖显式传入的 spec/options（options 缺省沿用旧值），
/// 未找到报 NOT_FOUND；写回经 write_config（带备份）。
pub(crate) fn update_plugin_entry(
    id: &str,
    updates_spec: Option<&Value>,
    updates_options: Option<&Value>,
    working_directory: Option<&Path>,
) -> Result<(), CodedError> {
    let Some(target) = find_plugin_target(id, working_directory)? else {
        return Err(coded("Plugin entry not found", CODE_NOT_FOUND));
    };
    let layers = read_plugin_config_layers(working_directory)?;
    let mut sources = config_sources(&layers);
    let source = &mut sources[target.source_index];
    let raw = source
        .config
        .get("plugin")
        .and_then(Value::as_array)
        .and_then(|items| items.get(target.plugin_index))
        .cloned()
        .ok_or_else(|| coded("Plugin entry not found", CODE_NOT_FOUND))?;
    let existing = parse_plugin_raw(&raw)?;
    let next_spec = match updates_spec {
        None => existing.spec,
        Some(spec) => validate_plugin_spec(spec)?,
    };
    let next_options = updates_options.cloned().or(existing.options);
    let serialized = serialize_plugin_entry(&next_spec, next_options.as_ref());
    if let Some(items) = source
        .config
        .get_mut("plugin")
        .and_then(Value::as_array_mut)
    {
        items[target.plugin_index] = serialized;
    }
    let config = std::mem::take(&mut source.config);
    let file_path = source.file_path.clone();
    write_config(&config, &file_path)
}

/// 按 id 删除插件条目；数组删空时连同 plugin 键一起移除，未找到报 NOT_FOUND。
pub(crate) fn delete_plugin_entry(
    id: &str,
    working_directory: Option<&Path>,
) -> Result<(), CodedError> {
    let Some(target) = find_plugin_target(id, working_directory)? else {
        return Err(coded("Plugin entry not found", CODE_NOT_FOUND));
    };
    let layers = read_plugin_config_layers(working_directory)?;
    let mut sources = config_sources(&layers);
    let source = &mut sources[target.source_index];
    let mut plugin = source
        .config
        .get("plugin")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if target.plugin_index >= plugin.len() {
        return Err(coded("Plugin entry not found", CODE_NOT_FOUND));
    }
    plugin.remove(target.plugin_index);
    if plugin.is_empty() {
        source.config.remove("plugin");
    } else {
        source.config.insert("plugin".into(), Value::Array(plugin));
    }
    let config = std::mem::take(&mut source.config);
    write_config(&config, &source.file_path)
}

/// 枚举各 scope 插件目录下的散落插件文件（user 恒定、有工作目录时追加 project），
/// 跳过子目录与不合命名规则的条目，按 scope 与文件名排序，输出带
/// id/fileName/scope/kind/absolutePath 的对象。
pub(crate) fn list_plugin_dir_files(
    working_directory: Option<&Path>,
) -> Result<Vec<Value>, CodedError> {
    let mut scopes = vec![SCOPE_USER];
    if working_directory.is_some() {
        scopes.push(SCOPE_PROJECT);
    }
    let mut files = Vec::new();
    for scope in scopes {
        let dir = plugin_dir_for_scope(scope, working_directory)?;
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut names: Vec<(String, bool)> = entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                let is_file = entry.file_type().ok()?.is_file();
                Some((name, is_file))
            })
            .collect();
        names.sort();
        for (name, is_file) in names {
            if !is_file || !plugin_file_name_ok(&name) || name.contains("..") {
                continue;
            }
            let mut file = Map::new();
            file.insert(
                "id".into(),
                Value::from(encode_plugin_id("file", &format!("{scope}:{name}"))),
            );
            file.insert("fileName".into(), Value::from(name.clone()));
            file.insert("scope".into(), Value::from(scope));
            file.insert("kind".into(), Value::from("file"));
            file.insert(
                "absolutePath".into(),
                Value::from(dir.join(&name).to_string_lossy().as_ref()),
            );
            files.push(Value::Object(file));
        }
    }
    Ok(files)
}

/// 读取到的插件文件：文件名、scope 与全文内容。
pub(crate) struct PluginDirFile {
    /// 已通过校验的插件文件名。
    pub file_name: String,
    /// 文件所属 scope（user 或 project）。
    pub scope: String,
    /// 文件全文内容。
    pub content: String,
}

/// 按 "file:..." id 读取插件文件；文件不存在返回 Ok(None)，读取失败报 IO。
pub(crate) fn read_plugin_dir_file(
    id: &str,
    working_directory: Option<&Path>,
) -> Result<Option<PluginDirFile>, CodedError> {
    let target = file_target_from_id(id, working_directory)?;
    if !target.absolute_path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&target.absolute_path)
        .map_err(|_| coded("Failed to read plugin file", "IO"))?;
    Ok(Some(PluginDirFile {
        file_name: target.file_name,
        scope: target.scope,
        content,
    }))
}

/// 写入插件文件：校验文件名与 scope；overwrite=false 且已存在时报 FILE_EXISTS；
/// content 为 null 写空串、字符串原样、其余 JSON 序列化；必要时创建目录，写失败报 IO。
pub(crate) fn write_plugin_dir_file(
    file_name: &Value,
    content: &Value,
    scope: Option<&str>,
    working_directory: Option<&Path>,
    overwrite: bool,
) -> Result<(), CodedError> {
    let file_name = validate_file_name(file_name.as_str().unwrap_or(""))?;
    let scope = scope.unwrap_or(SCOPE_USER);
    validate_scope(scope)?;
    let dir = plugin_dir_for_scope(scope, working_directory)?;
    let absolute_path = dir.join(&file_name);
    if !overwrite && absolute_path.exists() {
        return Err(coded(
            format!("Plugin file \"{file_name}\" already exists"),
            CODE_FILE_EXISTS,
        ));
    }
    std::fs::create_dir_all(&dir).map_err(|_| coded("Failed to write plugin file", "IO"))?;
    let body = match content {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    std::fs::write(&absolute_path, body).map_err(|_| coded("Failed to write plugin file", "IO"))?;
    Ok(())
}

/// 按 "file:..." id 删除插件文件；文件不存在报 NOT_FOUND，删除失败报 IO。
pub(crate) fn delete_plugin_dir_file(
    id: &str,
    working_directory: Option<&Path>,
) -> Result<(), CodedError> {
    let target = file_target_from_id(id, working_directory)?;
    if !target.absolute_path.exists() {
        return Err(coded(
            format!("Plugin file \"{}\" not found", target.file_name),
            CODE_NOT_FOUND,
        ));
    }
    std::fs::remove_file(&target.absolute_path)
        .map_err(|_| coded("Failed to delete plugin file", "IO"))?;
    Ok(())
}

/// plugins.rs 的单元测试：覆盖条目解析/序列化、CRUD、scope 路由、id 编解码与
/// 插件目录文件管理；全部经 EnvGuard 串行并隔离 HOME/OPENCODE_CONFIG。
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 测试夹具：持有全局环境锁，保存并在 Drop 时恢复 HOME 与 OPENCODE_CONFIG。
    struct EnvGuard {
        /// http_util::TEST_ENV_MUTEX 的互斥锁守卫，Drop 时释放（串行化所有动环境的测试）。
        _guard: std::sync::MutexGuard<'static, ()>,
        /// 进入测试前的 HOME 原值。
        previous_home: Option<String>,
        /// Windows home 解析优先 USERPROFILE（os.homedir 对齐），一并隔离。
        previous_userprofile: Option<String>,
        /// 进入测试前的 OPENCODE_CONFIG 原值。
        previous_config: Option<String>,
    }

    /// Drop 实现：测试结束时恢复被改写的环境变量。
    impl Drop for EnvGuard {
        /// 依次恢复 HOME 与 OPENCODE_CONFIG。
        fn drop(&mut self) {
            restore("HOME", &self.previous_home);
            restore("USERPROFILE", &self.previous_userprofile);
            restore("OPENCODE_CONFIG", &self.previous_config);
        }
    }

    /// 把环境变量恢复为保存值；未保存则移除（unsafe set_var/remove_var 仅在持锁的单测试内使用）。
    fn restore(key: &str, value: &Option<String>) {
        match value {
            Some(value) => unsafe {
                std::env::set_var(key, value);
            },
            None => unsafe {
                std::env::remove_var(key);
            },
        }
    }

    /// EnvGuard 的加锁与环境设置逻辑。
    impl EnvGuard {
        /// 获取全局环境锁并按需设置 HOME/OPENCODE_CONFIG（custom 为 None 时清空两者），返回守卫。
        fn lock(custom: Option<&Path>) -> Self {
            let guard = super::super::http_util::TEST_ENV_MUTEX
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let previous_home = std::env::var("HOME").ok();
            // Windows resolves home through USERPROFILE first (os.homedir
            // parity, fed62e53) — isolate both so the fixture home wins.
            let previous_userprofile = std::env::var("USERPROFILE").ok();
            let previous_config = std::env::var("OPENCODE_CONFIG").ok();
            if let Some(path) = custom {
                let home = path
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from("/"));
                unsafe {
                    std::env::set_var("HOME", home.to_string_lossy().as_ref());
                    std::env::set_var("USERPROFILE", home.to_string_lossy().as_ref());
                    std::env::set_var("OPENCODE_CONFIG", path.to_string_lossy().as_ref());
                }
            } else {
                unsafe {
                    std::env::remove_var("HOME");
                    std::env::remove_var("USERPROFILE");
                    std::env::remove_var("OPENCODE_CONFIG");
                }
            }
            Self {
                _guard: guard,
                previous_home,
                previous_userprofile,
                previous_config,
            }
        }
    }

    /// 为当前测试创建唯一的临时根目录（进程 id + 原子计数防碰撞）。
    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ompchamber-plugins-{}-{tag}-{}",
            std::process::id(),
            rand_postfix()
        ));
        std::fs::create_dir_all(&dir).expect("temp root");
        dir
    }

    /// 进程内原子递增计数，用于临时目录去重。
    fn rand_postfix() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        // 局部静态计数器：每次调用递增，保证并发测试目录唯一。
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        COUNTER.fetch_add(1, Ordering::SeqCst)
    }

    /// 测试助手：把 JSON 值 pretty 写入指定路径（自动创建父目录）。
    fn write_json(path: &Path, data: Value) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, serde_json::to_string_pretty(&data).expect("json")).expect("write");
    }

    /// 测试助手：读取并解析路径上的 JSON 文件。
    fn read_json(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).expect("read")).expect("parse")
    }

    /// 验证 parse_plugin_raw 接受字符串与 [spec, options] 元组、拒绝其它形状，
    /// 且 serialize_plugin_entry 仅在 options 非空对象时输出元组。
    #[test]
    fn parses_and_serializes_raw_entries() {
        assert_eq!(parse_plugin_raw(&json!("foo")).expect("parse").spec, "foo");
        let tuple = parse_plugin_raw(&json!(["foo", {"a": 1}])).expect("parse");
        assert_eq!(tuple.spec, "foo");
        assert_eq!(tuple.options, Some(json!({"a": 1})));
        assert_eq!(
            parse_plugin_raw(&json!(["foo", {}]))
                .expect("parse")
                .options,
            Some(json!({}))
        );
        let error = parse_plugin_raw(&json!(123)).expect_err("must throw");
        assert!(error.message.contains("Plugin spec"));

        assert_eq!(serialize_plugin_entry("foo", None), json!("foo"));
        assert_eq!(
            serialize_plugin_entry("foo", Some(&json!({}))),
            json!("foo")
        );
        assert_eq!(
            serialize_plugin_entry("foo", Some(&json!({"a": 1}))),
            json!(["foo", {"a": 1}])
        );
    }

    /// 验证非字符串 spec 与穿越/大写/非法扩展名的文件名分别被 INVALID_SPEC / INVALID_FILENAME 拒绝。
    #[test]
    fn rejects_invalid_specs_and_file_names() {
        let root = temp_root("invalid");
        let _env = EnvGuard::lock(Some(&root.join("user-opencode.json")));
        let project = root.join("project");
        std::fs::create_dir_all(&project).expect("mkdir");

        let error = create_plugin_entry(&json!(123), None, Some("user"), Some(&project))
            .expect_err("must throw");
        assert!(error.message.contains("Plugin spec"));

        for name in ["", "../bad.js", "a/b.js", "A.js", "foo.txt"] {
            let error = write_plugin_dir_file(
                &json!(name),
                &json!(""),
                Some("project"),
                Some(&project),
                false,
            )
            .expect_err("must throw");
            assert!(error.message.contains("Plugin file name"), "name: {name}");
        }
        std::fs::remove_dir_all(&root).ok();
    }

    /// 验证创建纯字符串与带 options 的条目均写入用户配置，且同 scope 重复 spec 返回 ENTRY_EXISTS。
    #[test]
    fn creates_entries_and_rejects_duplicates() {
        let root = temp_root("create");
        let user_config = root.join("user-opencode.json");
        let _env = EnvGuard::lock(Some(&user_config));
        let project = root.join("project");
        std::fs::create_dir_all(&project).expect("mkdir");

        create_plugin_entry(&json!("plain-plugin"), None, Some("user"), Some(&project))
            .expect("create");
        create_plugin_entry(
            &json!("tuple-plugin"),
            Some(&json!({"apiKey": "x"})),
            Some("user"),
            Some(&project),
        )
        .expect("create");

        assert_eq!(
            read_json(&user_config).get("plugin").cloned(),
            Some(json!(["plain-plugin", ["tuple-plugin", {"apiKey": "x"}]]))
        );
        let error = create_plugin_entry(&json!("plain-plugin"), None, Some("user"), Some(&project))
            .expect_err("must throw");
        assert!(error.message.contains("already exists"));
        assert_eq!(error.code, "ENTRY_EXISTS");
        std::fs::remove_dir_all(&root).ok();
    }

    /// 验证 user 与 project scope 的条目分别写入用户配置与 <project>/.opencode/opencode.json。
    #[test]
    fn routes_entries_by_scope() {
        let root = temp_root("scopes");
        let user_config = root.join("user-opencode.json");
        let _env = EnvGuard::lock(Some(&user_config));
        let project = root.join("project");
        std::fs::create_dir_all(&project).expect("mkdir");

        create_plugin_entry(&json!("user-plugin"), None, Some("user"), Some(&project))
            .expect("create");
        create_plugin_entry(
            &json!("project-plugin"),
            None,
            Some("project"),
            Some(&project),
        )
        .expect("create");

        assert_eq!(
            read_json(&user_config).get("plugin").cloned(),
            Some(json!(["user-plugin"]))
        );
        assert_eq!(
            read_json(&project.join(".opencode").join("opencode.json"))
                .get("plugin")
                .cloned(),
            Some(json!(["project-plugin"]))
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// 验证 OPENCODE_CONFIG 在每次调用间被重新读取：两个环境各自写入自己的配置与 plugins 目录，互不串扰。
    #[test]
    fn re_resolves_custom_config_env_between_calls() {
        let root = temp_root("reenv");
        let first = root.join("first").join("opencode.json");
        let second = root.join("second").join("opencode.json");
        let project = root.join("project");
        std::fs::create_dir_all(&project).expect("mkdir");

        {
            let _env = EnvGuard::lock(Some(&first));
            create_plugin_entry(&json!("first-plugin"), None, Some("user"), Some(&project))
                .expect("create");
            write_plugin_dir_file(
                &json!("first.js"),
                &json!("one"),
                Some("user"),
                Some(&project),
                false,
            )
            .expect("write");
        }
        {
            let _env = EnvGuard::lock(Some(&second));
            create_plugin_entry(&json!("second-plugin"), None, Some("user"), Some(&project))
                .expect("create");
            write_plugin_dir_file(
                &json!("second.js"),
                &json!("two"),
                Some("user"),
                Some(&project),
                false,
            )
            .expect("write");
        }

        assert_eq!(
            read_json(&first).get("plugin").cloned(),
            Some(json!(["first-plugin"]))
        );
        assert_eq!(
            read_json(&second).get("plugin").cloned(),
            Some(json!(["second-plugin"]))
        );
        assert!(
            first
                .parent()
                .unwrap()
                .join("plugins")
                .join("first.js")
                .exists()
        );
        assert!(
            second
                .parent()
                .unwrap()
                .join("plugins")
                .join("second.js")
                .exists()
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// 验证按 id 更新在原下标替换条目：spec 与 options 可独立更新，元组/字符串形态随 options 有无切换。
    #[test]
    fn updates_entries_in_place() {
        let root = temp_root("update");
        let user_config = root.join("user-opencode.json");
        let _env = EnvGuard::lock(Some(&user_config));
        let project = root.join("project");
        std::fs::create_dir_all(&project).expect("mkdir");
        write_json(
            &user_config,
            json!({"plugin": ["first", ["second", {"a": 1}], "third"]}),
        );

        update_plugin_entry(
            &encode_plugin_id("config", "user:second"),
            Some(&json!("second-new")),
            Some(&json!({})),
            Some(&project),
        )
        .expect("update");
        assert_eq!(
            read_json(&user_config).get("plugin").cloned(),
            Some(json!(["first", "second-new", "third"]))
        );

        update_plugin_entry(
            &encode_plugin_id("config", "user:first"),
            Some(&json!("first-new")),
            Some(&json!({"b": 2})),
            Some(&project),
        )
        .expect("update");
        assert_eq!(
            read_json(&user_config).get("plugin").cloned(),
            Some(json!([["first-new", {"b": 2}], "second-new", "third"]))
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// 验证删除唯一条目后 plugin 键被整体移除，配置文件退化为空对象。
    #[test]
    fn deletes_entries_and_prunes_plugin_key() {
        let root = temp_root("delete");
        let user_config = root.join("user-opencode.json");
        let _env = EnvGuard::lock(Some(&user_config));
        let project = root.join("project");
        std::fs::create_dir_all(&project).expect("mkdir");
        write_json(&user_config, json!({"plugin": ["only"]}));

        delete_plugin_entry(&encode_plugin_id("config", "user:only"), Some(&project))
            .expect("delete");
        assert_eq!(read_json(&user_config), json!({}));
        std::fs::remove_dir_all(&root).ok();
    }

    /// 验证项目配置 JSONC 损坏时列表降级为仅用户层，且不改动也不备份损坏文件。
    #[test]
    fn lists_user_plugins_when_project_layer_is_unparseable() {
        let root = temp_root("unparseable");
        let user_config = root.join("user-opencode.json");
        let _env = EnvGuard::lock(Some(&user_config));
        let project = root.join("project");
        let project_file = project.join(".opencode").join("opencode.jsonc");
        let partial = "{\n  \"$schema\": \"https://opencode.ai/config.json\",\n  plugin: [\"broken-project-plugin\"],\n}\n";
        write_json(&user_config, json!({"plugin": ["user-plugin"]}));
        std::fs::create_dir_all(project_file.parent().unwrap()).expect("mkdir");
        std::fs::write(&project_file, partial).expect("write");

        let entries = list_plugin_entries(Some(&project)).expect("list");
        assert_eq!(
            entries
                .iter()
                .map(|e| e["spec"].clone())
                .collect::<Vec<_>>(),
            vec![json!("user-plugin")]
        );
        assert_eq!(
            std::fs::read_to_string(&project_file).expect("read"),
            partial
        );
        assert!(
            !project
                .join(".opencode")
                .join("opencode.jsonc.ompchamber.backup")
                .exists()
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// 验证列表跨 user/project 两层输出 scope/kind/parsedKind/sourcePath，用户配置移除后只剩项目层。
    #[test]
    fn lists_entries_with_scopes_and_parsed_kinds() {
        let root = temp_root("list");
        let user_config = root.join("user-opencode.json");
        let _env = EnvGuard::lock(Some(&user_config));
        let project = root.join("project");
        let project_config = project.join(".opencode").join("opencode.json");
        std::fs::create_dir_all(&project).expect("mkdir");
        write_json(
            &user_config,
            json!({"plugin": ["npm-plugin", "/abs/plugin.js", "@scope/pkg@1.0.0"]}),
        );
        write_json(&project_config, json!({"plugin": ["./local-plugin.js"]}));

        let entries = list_plugin_entries(Some(&project)).expect("list");
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0]["spec"], json!("npm-plugin"));
        assert_eq!(entries[0]["scope"], json!("user"));
        assert_eq!(entries[0]["kind"], json!("config"));
        assert_eq!(entries[0]["parsedKind"], json!("npm"));
        assert_eq!(
            entries[0]["sourcePath"],
            json!(user_config.to_string_lossy().as_ref())
        );
        assert_eq!(entries[1]["parsedKind"], json!("path"));
        assert_eq!(entries[2]["parsedKind"], json!("npm"));
        assert_eq!(entries[3]["scope"], json!("project"));
        assert_eq!(
            entries[3]["sourcePath"],
            json!(project_config.to_string_lossy().as_ref())
        );

        std::fs::remove_file(&user_config).expect("remove");
        let entries = list_plugin_entries(Some(&project)).expect("list");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["spec"], json!("./local-plugin.js"));
        std::fs::remove_dir_all(&root).ok();
    }

    /// 验证 encode/decode_plugin_id 对含 @ 与冒号的 spec 往返一致，纯垃圾输入报 "Invalid plugin id"。
    #[test]
    fn encodes_and_decodes_ids() {
        let id = encode_plugin_id("config", "user:oh-my-openagent@4.3.0");
        assert_eq!(
            decode_plugin_id(&id).expect("decode"),
            (
                "config".to_string(),
                "user:oh-my-openagent@4.3.0".to_string()
            )
        );
        let error = decode_plugin_id("%%%").expect_err("must throw");
        assert_eq!(error.message, "Invalid plugin id");
    }

    /// 验证插件文件写入—列表—读取—删除全链路，删除后再次删除报 NOT_FOUND。
    #[test]
    fn round_trips_plugin_dir_files() {
        let root = temp_root("files");
        let _env = EnvGuard::lock(Some(&root.join("user-opencode.json")));
        let project = root.join("project");
        std::fs::create_dir_all(&project).expect("mkdir");

        write_plugin_dir_file(
            &json!("my-plugin.ts"),
            &json!("export default {}"),
            Some("project"),
            Some(&project),
            false,
        )
        .expect("write");
        let files = list_plugin_dir_files(Some(&project)).expect("list");
        let file = files
            .iter()
            .find(|f| f["fileName"] == json!("my-plugin.ts"))
            .expect("file listed");
        assert_eq!(file["scope"], json!("project"));
        assert_eq!(file["kind"], json!("file"));

        let id = file["id"].as_str().expect("id").to_string();
        let read = read_plugin_dir_file(&id, Some(&project))
            .expect("read")
            .expect("present");
        assert_eq!(read.file_name, "my-plugin.ts");
        assert_eq!(read.scope, "project");
        assert_eq!(read.content, "export default {}");

        delete_plugin_dir_file(&id, Some(&project)).expect("delete");
        let remaining = list_plugin_dir_files(Some(&project)).expect("list");
        assert!(remaining.iter().all(|f| f["scope"] != json!("project")));
        let error = delete_plugin_dir_file(&id, Some(&project)).expect_err("must throw");
        assert!(error.message.contains("not found"));
        std::fs::remove_dir_all(&root).ok();
    }

    /// 验证同名文件二次写入报 FILE_EXISTS，overwrite=true 时允许覆盖并落盘新内容。
    #[test]
    fn rejects_duplicate_files_unless_overwrite() {
        let root = temp_root("dupfile");
        let _env = EnvGuard::lock(Some(&root.join("user-opencode.json")));
        let project = root.join("project");
        std::fs::create_dir_all(&project).expect("mkdir");

        write_plugin_dir_file(
            &json!("dup.js"),
            &json!("one"),
            Some("project"),
            Some(&project),
            false,
        )
        .expect("write");
        let error = write_plugin_dir_file(
            &json!("dup.js"),
            &json!("two"),
            Some("project"),
            Some(&project),
            false,
        )
        .expect_err("must throw");
        assert!(error.message.contains("already exists"));
        assert_eq!(error.code, "FILE_EXISTS");

        write_plugin_dir_file(
            &json!("dup.js"),
            &json!("two"),
            Some("project"),
            Some(&project),
            true,
        )
        .expect("overwrite");
        assert_eq!(
            std::fs::read_to_string(project.join(".opencode").join("plugins").join("dup.js"))
                .expect("read"),
            "two"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// 验证插件目录只列出符合命名规则的普通文件（README.md 之类的条目被过滤）。
    #[test]
    fn lists_only_valid_plugin_dir_files() {
        let root = temp_root("validfiles");
        let _env = EnvGuard::lock(Some(&root.join("user-opencode.json")));
        let project = root.join("project");
        let dir = project.join(".opencode").join("plugins");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("valid.mjs"), "").expect("write");
        std::fs::write(dir.join("README.md"), "").expect("write");

        let files = list_plugin_dir_files(Some(&project)).expect("list");
        let project_files: Vec<&Value> = files
            .iter()
            .filter(|f| f["scope"] == json!("project"))
            .collect();
        assert_eq!(project_files.len(), 1);
        assert_eq!(project_files[0]["fileName"], json!("valid.mjs"));
        std::fs::remove_dir_all(&root).ok();
    }
}
