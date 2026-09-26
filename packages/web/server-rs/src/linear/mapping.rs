//! Port of `server/lib/linear/mapping.js` — Linear-team-to-project mapping
//! storage, kept separate from the auth file so a disconnect does not wipe
//! maps. Reads and writes use the current workspace's slice.

use std::path::PathBuf;

use serde_json::{Value, json};

use super::issues::{LinearTeam, read_team};
use super::parse::{is_plain_object, read_trimmed_string};
use super::{LinearError, LinearState};

fn mapping_org_key(state: &LinearState) -> String {
    state
        .get_auth()
        .map(|auth| auth.workspace_id)
        .filter(|key| !key.is_empty())
        .unwrap_or_else(|| "__unscoped__".to_string())
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct MappingSlice {
    pub default_project_path: Option<String>,
    pub team_project_paths: Vec<(String, String)>,
}

impl MappingSlice {
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

    fn team_path(&self, team_id: &str) -> Option<&String> {
        self.team_project_paths
            .iter()
            .find(|(key, _)| key == team_id)
            .map(|(_, value)| value)
    }
}

fn empty_mapping() -> MappingSlice {
    MappingSlice::default()
}

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

fn normalize_mapping_slice(raw: &Value) -> MappingSlice {
    if !is_plain_object(raw) {
        return empty_mapping();
    }
    MappingSlice {
        default_project_path: optional(&raw["defaultProjectPath"]),
        team_project_paths: read_team_project_paths(&raw["teamProjectPaths"]),
    }
}

fn optional(value: &Value) -> Option<String> {
    let trimmed = read_trimmed_string(value);
    (!trimmed.is_empty()).then_some(trimmed)
}

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

impl LinearState {
    pub fn mapping_file(&self) -> PathBuf {
        self.data_dir.join("linear-mapping.json")
    }

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

/// Decode the `teams` array from `listLinearTeams` output.
pub fn teams_from_json(value: &Value) -> Vec<LinearTeam> {
    value
        .as_array()
        .map(|nodes| nodes.iter().filter_map(read_team).collect())
        .unwrap_or_default()
}

/// The merged mapping view the UI consumes (JS `mergeLinearMappingView`).
#[derive(Debug, Clone, PartialEq)]
pub struct MappingView {
    pub default_project_path: Option<String>,
    pub teams: Vec<MappingViewTeam>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MappingViewTeam {
    pub id: String,
    pub key: String,
    pub name: String,
    pub project_path: Option<String>,
}

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

impl MappingView {
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

/// A team reference for [`resolve_mapped_project_path`].
#[derive(Debug, Clone, Default)]
pub struct TeamRef {
    pub id: String,
    pub key: String,
}

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
