//! Port of `server/lib/linear/mapping.js` — Linear-team-to-project mapping
//! storage, kept separate from the auth file so a disconnect does not wipe
//! maps. Reads and writes use the current workspace's slice.
//! 本模块是 `server/lib/linear/mapping.js` 的 Rust 移植：Linear team 到
//! project 的映射存储，与授权文件分开保存，断开连接不会清掉映射。
//! 读写都只作用于当前 workspace 对应的分片。

use std::path::PathBuf;

use serde_json::{Value, json};

use super::issues::{LinearTeam, read_team};
use super::parse::{is_plain_object, read_trimmed_string};
use super::{LinearError, LinearState};

/// 当前 workspace 的映射键：取授权条目的 workspace_id，未连接（或为空）时
/// 回落到 "__unscoped__"。
fn mapping_org_key(state: &LinearState) -> String {
    state
        .get_auth()
        .map(|auth| auth.workspace_id)
        .filter(|key| !key.is_empty())
        .unwrap_or_else(|| "__unscoped__".to_string())
}

/// 单个 workspace 的映射分片：默认 project 路径 + 各 team 到 project 路径的有序对。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MappingSlice {
    /// 未匹配到具体 team 时使用的默认 project 路径。
    pub default_project_path: Option<String>,
    /// (team_id, project_path) 列表，保持写入顺序。
    pub team_project_paths: Vec<(String, String)>,
}

/// 分片的 JSON 序列化与按 team 查询。
impl MappingSlice {
    /// 序列化为 JS 侧文件里的分片结构：defaultProjectPath + teamProjectPaths 对象。
    fn to_json(&self) -> Value {
        let mut teams = serde_json::Map::new();
        for (team_id, path) in &self.team_project_paths {
            teams.insert(team_id.clone(), json!(path));
        }
        json!({
            "defaultProjectPath": self.default_project_path,
            "teamProjectPaths": Value::Object(teams),
        })
    }

    /// 按 team id 精确查找已映射的 project 路径。
    fn team_path(&self, team_id: &str) -> Option<&String> {
        self.team_project_paths
            .iter()
            .find(|(key, _)| key == team_id)
            .map(|(_, value)| value)
    }
}

/// 返回全空的映射分片。
fn empty_mapping() -> MappingSlice {
    MappingSlice::default()
}

/// 对应 JS readTeamProjectPaths：只保留 trim 后非空的 team id 与 project 路径。
/// JS `readTeamProjectPaths`: keeps only non-empty keys and values.
fn read_team_project_paths(value: &Value) -> Vec<(String, String)> {
    let Some(object) = value.as_object() else {
        return Vec::new();
    };
    let mut next = Vec::new();
    for (key, raw) in object {
        let team_id = key.trim();
        let project_path = read_trimmed_string(raw);
        if !team_id.is_empty() && !project_path.is_empty() {
            next.push((team_id.to_string(), project_path));
        }
    }
    next
}

/// 把任意 JSON 值归一化为 MappingSlice：非对象一律按空映射处理。
fn normalize_mapping_slice(raw: &Value) -> MappingSlice {
    if !is_plain_object(raw) {
        return empty_mapping();
    }
    MappingSlice {
        default_project_path: optional(&raw["defaultProjectPath"]),
        team_project_paths: read_team_project_paths(&raw["teamProjectPaths"]),
    }
}

/// 读取 trim 后非空的字符串，否则返回 None。
fn optional(value: &Value) -> Option<String> {
    let trimmed = read_trimmed_string(value);
    (!trimmed.is_empty()).then_some(trimmed)
}

/// 对应 JS readMappingDocument：含 workspaces 对象的新版文档原样透传各分片
///（跳过空键）；旧版扁平文件则把自己包在当前 workspace 键下成为单分片。
/// JS `readMappingDocument`: a `workspaces` object passes through; a legacy
/// flat file wraps itself under the current workspace key.
fn read_mapping_document(state: &LinearState, raw: &Value) -> Vec<(String, MappingSlice)> {
    if let Some(workspaces) = raw["workspaces"].as_object() {
        let mut document = Vec::new();
        for (key, slice) in workspaces {
            let org_key = key.trim();
            if org_key.is_empty() {
                continue;
            }
            document.push((org_key.to_string(), normalize_mapping_slice(slice)));
        }
        return document;
    }
    vec![(mapping_org_key(state), normalize_mapping_slice(raw))]
}

/// LinearState 上的映射文件读写扩展。
impl LinearState {
    /// 映射文件路径：<data-dir>/linear-mapping.json。
    pub fn mapping_file(&self) -> PathBuf {
        self.data_dir.join("linear-mapping.json")
    }

    /// 对应 JS readStoredLinearMapping：读取映射文件并返回当前 workspace 的
    /// 分片；文件缺失或内容为空返回空映射；读取或解析失败报 MALFORMED。
    /// JS `readStoredLinearMapping`.
    pub fn read_stored_mapping(&self) -> Result<MappingSlice, LinearError> {
        let file_path = self.mapping_file();
        if !file_path.exists() {
            return Ok(empty_mapping());
        }
        let raw = std::fs::read_to_string(&file_path)
            .map_err(|_| LinearError::mapping("Linear mapping file is malformed"))?;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(empty_mapping());
        }
        let parsed: Value = serde_json::from_str(trimmed)
            .map_err(|_| LinearError::mapping("Linear mapping file is malformed"))?;
        if !is_plain_object(&parsed) {
            return Err(LinearError::mapping("Linear mapping file is malformed"));
        }
        let org_key = mapping_org_key(self);
        let document = read_mapping_document(self, &parsed);
        Ok(document
            .into_iter()
            .find(|(key, _)| *key == org_key)
            .map(|(_, slice)| slice)
            .unwrap_or_else(empty_mapping))
    }

    /// 对应 JS setStoredLinearMapping：写入当前 workspace 的分片并保留其它
    /// workspace 的分片，原子落盘后返回新分片；入参非对象报 INVALID，
    /// 已有文件损坏报 MALFORMED。
    /// JS `setStoredLinearMapping`.
    pub fn set_stored_mapping(&self, input: &Value) -> Result<MappingSlice, LinearError> {
        if !is_plain_object(input) {
            return Err(LinearError::invalid("Mapping body must be an object"));
        }
        let file_path = self.mapping_file();
        let mut document: Vec<(String, MappingSlice)> = Vec::new();
        if file_path.exists() {
            let raw = std::fs::read_to_string(&file_path)
                .map_err(|_| LinearError::mapping("Linear mapping file is malformed"))?;
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                let parsed: Value = serde_json::from_str(trimmed)
                    .map_err(|_| LinearError::mapping("Linear mapping file is malformed"))?;
                if !is_plain_object(&parsed) {
                    return Err(LinearError::mapping("Linear mapping file is malformed"));
                }
                document = read_mapping_document(self, &parsed);
            }
        }
        let next = MappingSlice {
            default_project_path: optional(&input["defaultProjectPath"]),
            team_project_paths: read_team_project_paths(&input["teamProjectPaths"]),
        };
        let org_key = mapping_org_key(self);
        match document.iter_mut().find(|(key, _)| *key == org_key) {
            Some(entry) => entry.1 = next.clone(),
            None => document.push((org_key, next.clone())),
        }
        let payload = json!({
            "workspaces": document
                .iter()
                .map(|(key, slice)| (key.clone(), slice.to_json()))
                .collect::<serde_json::Map<String, Value>>()
        });
        let body = serde_json::to_string_pretty(&payload)
            .map_err(|e| LinearError::plain(e.to_string()))?;
        super::write_file_atomic_600(&file_path, &body)
            .map_err(|e| LinearError::plain(e.to_string()))?;
        Ok(next)
    }
}

/// 从 listLinearTeams 输出的 teams 数组解码 LinearTeam 列表（跳过无法解析的节点）。
/// Decode the `teams` array from `listLinearTeams` output.
pub fn teams_from_json(value: &Value) -> Vec<LinearTeam> {
    value
        .as_array()
        .map(|nodes| nodes.iter().filter_map(read_team).collect())
        .unwrap_or_default()
}

/// UI 消费的合并视图（对应 JS mergeLinearMappingView）：映射 + team 列表。
/// The merged mapping view the UI consumes (JS `mergeLinearMappingView`).
#[derive(Debug, Clone, PartialEq)]
pub struct MappingView {
    /// 默认 project 路径（来自存储的映射）。
    pub default_project_path: Option<String>,
    /// 每个 team 的展示行（各自带上映射到的 project 路径）。
    pub teams: Vec<MappingViewTeam>,
}

/// 映射视图中单个 team 的展示行。
#[derive(Debug, Clone, PartialEq)]
pub struct MappingViewTeam {
    /// Linear team id。
    pub id: String,
    /// Linear team key（如 ENG）。
    pub key: String,
    /// Linear team 名称。
    pub name: String,
    /// 该 team 映射到的 project 路径；未映射时为 None。
    pub project_path: Option<String>,
}

/// 把存储的映射（可为空）与 team 列表合并成 MappingView：逐 team 附上
/// 其映射的 project 路径，默认路径单独立字段。
pub fn merge_linear_mapping_view(
    stored: Option<&MappingSlice>,
    teams: &[LinearTeam],
) -> MappingView {
    let empty = MappingSlice::default();
    let mapping = stored.unwrap_or(&empty);
    MappingView {
        default_project_path: mapping.default_project_path.clone(),
        teams: teams
            .iter()
            .map(|team| MappingViewTeam {
                id: team.id.clone(),
                key: team.key.clone(),
                name: team.name.clone(),
                project_path: mapping.team_path(&team.id).cloned(),
            })
            .collect(),
    }
}

/// 视图的 JSON 输出。
impl MappingView {
    /// 序列化为 UI 期望的 JSON 形态（defaultProjectPath + teams 数组）。
    pub fn to_json(&self) -> Value {
        json!({
            "defaultProjectPath": self.default_project_path,
            "teams": self.teams.iter().map(|team| json!({
                "id": team.id,
                "key": team.key,
                "name": team.name,
                "projectPath": team.project_path,
            })).collect::<Vec<_>>(),
        })
    }
}

/// 供 [`resolve_mapped_project_path`] 使用的 team 引用（id 与 key）。
/// A team reference for [`resolve_mapped_project_path`].
#[derive(Debug, Clone, Default)]
pub struct TeamRef {
    /// Linear team id（优先匹配）。
    pub id: String,
    /// Linear team key（id 未命中时匹配）。
    pub key: String,
}

/// 对应 JS resolveMappedProjectPath：依次按 team id、team key 匹配映射路径，
/// 均未命中（或未提供 team）时回落到默认 project 路径。
/// JS `resolveMappedProjectPath`: team id, then team key, then the default.
pub fn resolve_mapped_project_path(view: &MappingView, team: Option<&TeamRef>) -> Option<String> {
    if let Some(team) = team {
        let team_id = team.id.trim();
        if !team_id.is_empty()
            && let Some(row) = view.teams.iter().find(|entry| entry.id == team_id)
            && let Some(path) = row.project_path.as_deref().filter(|p| !p.is_empty())
        {
            return Some(path.to_string());
        }
        let team_key = team.key.trim();
        if !team_key.is_empty()
            && let Some(row) = view.teams.iter().find(|entry| entry.key == team_key)
            && let Some(path) = row.project_path.as_deref().filter(|p| !p.is_empty())
        {
            return Some(path.to_string());
        }
    }
    view.default_project_path.clone()
}
