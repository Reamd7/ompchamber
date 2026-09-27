//! Port of the `shared.js` config-layer machinery: JSONC-tolerant
//! config-file reads (user / project / custom layers), deep merge,
//! defensive writes with `.ompchamber.backup` backups, and the per-layer
//! entry-source / write-target resolution used by the agent, command, MCP,
//! and provider CRUD surfaces.
//! 中文说明:本模块移植 `shared.js` 的配置层机制:容忍 JSONC 的配置
//! 文件读取(user/project/custom 三层)、深度合并、带
//! `.ompchamber.backup` 备份的防御式写入,以及 agent、command、MCP、
//! provider 各 CRUD 面共用的条目来源解析与写入目标选择。

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::OpenCodeEnv;

/// Layer identity within [`ConfigLayers`].
/// 标识 [`ConfigLayers`] 中的某一配置层,用于按层读取内容、定位路径
/// 与写回修改。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LayerKind {
    /// 用户层:用户配置目录下的主配置文件(config/opencode JSONC)。
    User,
    /// 项目层:工作目录下的 opencode.json(c)。
    Project,
    /// 自定义层:环境显式指定的 custom 配置文件,优先级最高。
    Custom,
}

/// 单个配置层的读取错误记录:JSONC 解析失败(INVALID_JSONC)不会中断
/// 整体读取,而是记入 `ConfigLayers::layer_errors`,由调用方在必须
/// 读写该层时再显式抛出。
#[derive(Debug, Clone)]
pub(crate) struct LayerErrorEntry {
    /// 出错的配置文件路径(作为层身份标识参与匹配)。
    pub path: PathBuf,
    /// 错误码,目前恒为 "INVALID_JSONC"。
    pub code: String,
    /// 面向用户的错误消息。
    pub message: String,
}

/// 一次读取得到的三层配置快照:各层原始内容、深度合并后的生效配置、
/// 各层文件路径与读取中累积的层错误;是所有 CRUD 面观察配置世界的
/// 统一视图。
#[derive(Debug, Clone)]
pub(crate) struct ConfigLayers {
    /// 用户层解析结果(文件缺失或解析失败时为空映射)。
    pub user_config: Map<String, Value>,
    /// 项目层解析结果(项目路径可能不存在,映射恒有效)。
    pub project_config: Map<String, Value>,
    /// 自定义层解析结果(未配置 custom 路径时为空映射)。
    pub custom_config: Map<String, Value>,
    /// user ← project ← custom 依次深度合并后的生效配置。
    pub merged_config: Map<String, Value>,
    /// 用户层文件路径(恒存在)。
    pub user_path: PathBuf,
    /// 项目层文件路径(无工作目录时为 None)。
    pub project_path: Option<PathBuf>,
    /// 自定义层文件路径(环境未配置时为 None)。
    pub custom_path: Option<PathBuf>,
    /// 各层读取错误(INVALID_JSONC),按 user/project/custom 顺序入队。
    pub layer_errors: Vec<LayerErrorEntry>,
}

/// `ConfigLayers` 的层访问辅助:按 [`LayerKind`] 读写层内容、取文件
/// 路径、查询层错误。
impl ConfigLayers {
    /// 取指定层的只读配置映射。
    pub(crate) fn layer(&self, kind: LayerKind) -> &Map<String, Value> {
        match kind {
            LayerKind::User => &self.user_config,
            LayerKind::Project => &self.project_config,
            LayerKind::Custom => &self.custom_config,
        }
    }

    /// 取指定层的可变配置映射。
    pub(crate) fn layer_mut(&mut self, kind: LayerKind) -> &mut Map<String, Value> {
        match kind {
            LayerKind::User => &mut self.user_config,
            LayerKind::Project => &mut self.project_config,
            LayerKind::Custom => &mut self.custom_config,
        }
    }

    /// 取指定层的文件路径;user 层恒为 Some,project/custom 层可能为 None。
    pub(crate) fn layer_path(&self, kind: LayerKind) -> Option<PathBuf> {
        match kind {
            LayerKind::User => Some(self.user_path.clone()),
            LayerKind::Project => self.project_path.clone(),
            LayerKind::Custom => self.custom_path.clone(),
        }
    }

    /// 查找指定路径的层是否记录了读取错误,命中返回对应条目。
    fn layer_error(&self, path: &Path) -> Option<&LayerErrorEntry> {
        self.layer_errors.iter().find(|entry| entry.path == path)
    }

    /// `throwIfLayerError`.
    /// `throwIfLayerError`:该层若记录了错误则以克隆的消息返回 Err,
    /// 否则 Ok;用于在必须读写某层前,把累积的解析错误显式抛出。
    fn raise_layer_error(&self, path: &Path) -> Result<(), String> {
        match self.layer_error(path) {
            Some(entry) => Err(entry.message.clone()),
            None => Ok(()),
        }
    }
}

/// `getJsonEntrySource` result.
/// `getJsonEntrySource` 的结果:条目在 JSON 层中的定位信息与条目值
/// 的克隆。
pub(crate) struct JsonEntrySource {
    /// 条目是否存在于任何一层。
    pub exists: bool,
    /// Which layer holds the entry (only set when `exists`).
    /// 条目所在层(仅 `exists` 为真时有值)。
    pub kind: Option<LayerKind>,
    /// 条目所在层的文件路径(仅 `exists` 为真时有值)。
    pub path: Option<PathBuf>,
    /// Clone of the section value (`section` in the JS).
    /// 条目值的克隆(对应 JS 中的 `section`)。
    pub section: Option<Value>,
}

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

/// 列出项目层配置文件的候选路径(按优先级):根目录 opencode.json、
/// opencode.jsonc,再到 `.opencode/` 子目录下同名两文件;无工作
/// 目录时返回空表。
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
/// 解析项目层配置路径:取第一个实际存在的候选;都不存在时返回首个
/// 候选(作为新文件的默认写入位置);无工作目录返回 None。
pub(crate) fn project_config_path(working_directory: Option<&Path>) -> Option<PathBuf> {
    let candidates = project_config_candidates(working_directory);
    candidates
        .iter()
        .find(|candidate| candidate.exists())
        .cloned()
        .or_else(|| candidates.first().cloned())
}

/// `getConfigPaths` + `getPrimaryUserConfigPath`.
/// 一次解析 user 与 project 两层路径:用户层依次探测
/// config.json/opencode.json/opencode.jsonc,全不存在时用默认的
/// `config_file()`;项目层委托 `project_config_path`。
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

/// 把 JSONC 解析错误格式化为统一的 INVALID_JSONC 消息:附文件路径、
/// 错误类别与出错偏移量。
fn format_jsonc_parse_error(file_path: &Path, error: &jsonc_parser::errors::ParseError) -> String {
    let location = format!(" ({:?} at offset {})", error.kind(), error.range().start);
    format!(
        "OpenCode configuration at {} contains invalid JSONC and cannot be loaded safely{location}",
        file_path.display()
    )
}

/// `parseConfigObject` — strict: any parse error or non-object root raises
/// the INVALID_JSONC error instead of returning a partial tree.
/// 严格解析配置文本为对象映射:允许注释与尾逗号;注释/空白文件解析
/// 为空对象;任何解析错误或根不是对象都报 INVALID_JSONC 错误,
/// 绝不返回残缺的部分树。
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
/// 读取单个配置文件为对象映射:文件不存在或内容全空白视为 `{}`;
/// IO 失败映射为固定错误消息(原错误仅记日志);JSONC 解析错误原样
/// 上抛。
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
/// 判断值是否为 JSON 对象(对应 JS 的 `isPlainObject`)。
pub(crate) fn is_plain_object(value: &Value) -> bool {
    matches!(value, Value::Object(_))
}

/// `mergeConfigs` — recursive plain-object merge, override wins otherwise.
/// 递归合并两份配置:双方同为对象时逐键深合并,否则 override 整体
/// 覆盖(base 侧缺失亦然);任一输入非对象时直接返回 override 的克隆。
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

/// `merge_configs` 的 Map 版本:包装为 Value 合并后解包;正常不会
/// 走到非对象分支,防御式地包装为 `{ "merged": … }`。
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
/// 读取全部配置层:先解析三层路径,再逐层读取;INVALID_JSONC 错误
/// 记录到 layer_errors 而不中断(出错层以空配置参与合并),合并顺序
/// user ← project ← custom。当前实现下所有层错误均被记录、不上抛,
/// 恒返回完整快照。
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
/// 读取单层配置:路径为 None 视为空层;读取/解析失败记录 error 日志
/// 并以空配置 + 错误消息返回,是否上抛由调用方决定。
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
/// 按文件路径取对应层的配置映射:依次匹配 custom → project,未命中
/// (含路径为 None)一律落到 user 层。
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
/// 防御式写回配置:目标文件已存在且非空白时先完整解析一遍(解析
/// 失败即中止,绝不覆盖无法解析的文件),并把原文件备份为
/// `<path>.ompchamber.backup`;随后确保父目录存在并以 pretty JSON
/// 写入(原文件中的注释会丢失)。IO/序列化失败映射为固定错误消息。
pub(crate) fn write_config(config: &Map<String, Value>, file_path: &Path) -> Result<(), String> {
    // 统一把各类写失败映射为固定消息,与 JS 的报错文案对齐。
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

/// 计算配置文件的备份路径:原文件名追加 `.ompchamber.backup` 后缀。
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
/// 按层优先级解析条目来源:custom → project → user。custom 与 user
/// 层若记录了 INVALID_JSONC 错误会直接上抛;project 层出错时仅静默
/// 跳过、继续向后查找;条目未命中任何层时返回 `exists: false` 的
/// 空结果。
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

/// 取 `config.<section_key>[entry_name]` 的条目值克隆;段、条目任一
/// 缺失或不是对象路径时返回 None。
fn section_entry(
    config: &Map<String, Value>,
    section_key: &str,
    entry_name: &str,
) -> Option<Value> {
    config.get(section_key)?.get(entry_name).cloned()
}

/// `getJsonWriteTarget`: custom layer always wins, then project for a
/// project-preferred scope, then the user layer.
/// 选择 JSON 写入目标层:custom 层存在即中选(层错误会上抛);否则
/// `prefer_project` 为真且有项目层时选 project;兜底选 user(两者的
/// 层错误同样上抛)。返回 (层类型, 文件路径)。
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
/// JSON 写入目标(层类型 + 文件路径):JS 的 handler 从
/// `getJsonWriteTarget`/`getJsonEntrySource` 拿到的是配置对象的活
/// 引用;Rust 侧改为克隆层内容修改后,凭此结构定位写回的层与路径。
pub(crate) struct JsonWriteTarget {
    /// 目标层类型。
    pub kind: LayerKind,
    /// 目标层的文件路径。
    pub path: PathBuf,
}

/// Section-map mutator mirroring `config.<section>[name]` access: creates
/// missing section/entry maps and errors when an existing entry is not a
/// plain object (JS strict-mode assignment on a primitive throws).
/// 段-条目映射的可变访问,镜像 JS 的 `config.<section>[name]` 读写:
/// 段或条目缺失时自动创建空对象;已存在的段/条目不是对象则报错
/// (对应 JS 严格模式对原始值属性赋值抛 TypeError 的行为)。
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
/// 从段中删除条目:条目删空时连所在段一并修剪;返回是否实际删除了
/// 内容(`deleteJsonAgentEntry` 语义的通用化)。
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
/// 深删除条目内的单个字段:字段删空条目时修剪条目,条目删空段时
/// 修剪段(对应 JS `delete config.agent[name][field]` 的链式清理);
/// 返回是否实际删除。
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
