//! Port of `server/lib/opencode/skills.js` — skill discovery, config CRUD,
//! supporting files, and rename — plus the `shared.js` skill-file helpers it
//! uses (`walkSkillMdFiles`, `addSkillFromMdFile`, supporting-file IO with
//! path containment, `getAncestors`, `findWorktreeRoot`).
//!
//! The JS test injects an `os` object for a fake home; the port threads
//! `home` explicitly. `OPENCODE_CONFIG_DIR` and `XDG_CACHE_HOME` are read per
//! call like the JS.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use super::config_layers::read_config_layer;
use super::md_yaml::{parse_md_content, write_md_file};

pub(crate) const BUILT_IN_SKILL_LOCATION: &str = "<built-in>";
const SKILL_SCOPE_USER: &str = "user";
const SKILL_SCOPE_PROJECT: &str = "project";

pub(crate) fn opencode_config_dir(home: &Path) -> PathBuf {
    home.join(".config").join("opencode")
}

pub(crate) fn skill_dir(home: &Path) -> PathBuf {
    opencode_config_dir(home).join("skills")
}

/// `ensureDirs` — the four global OpenCode config directories.
pub(crate) fn ensure_dirs(home: &Path) {
    let config_dir = opencode_config_dir(home);
    for dir in ["agents", "commands", "skills"] {
        let target = config_dir.join(dir);
        if !target.exists() {
            std::fs::create_dir_all(&target).ok();
        }
    }
}

pub(crate) fn find_worktree_root(start_dir: &Path) -> Option<PathBuf> {
    let mut current = super::http_util::resolve_path(&start_dir.to_string_lossy());
    loop {
        if current.join(".git").exists() {
            return Some(current);
        }
        let parent = current.parent()?.to_path_buf();
        if parent == current {
            return None;
        }
        current = parent;
    }
}

pub(crate) fn get_ancestors(start_dir: &Path, stop_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut result = Vec::new();
    let mut current = super::http_util::resolve_path(&start_dir.to_string_lossy());
    let resolved_stop =
        stop_dir.map(|stop| super::http_util::resolve_path(&stop.to_string_lossy()));
    loop {
        result.push(current.clone());
        if let Some(stop) = &resolved_stop
            && current == *stop
        {
            break;
        }
        let Some(parent) = current.parent() else {
            break;
        };
        let parent = parent.to_path_buf();
        if parent == current {
            break;
        }
        current = parent;
    }
    result
}

fn ensure_project_skill_dir(working_directory: &Path) -> PathBuf {
    let project_skill_dir = working_directory.join(".opencode").join("skills");
    if !project_skill_dir.exists() {
        std::fs::create_dir_all(&project_skill_dir).ok();
    }
    let legacy = working_directory.join(".opencode").join("skill");
    if !legacy.exists() {
        std::fs::create_dir_all(&legacy).ok();
    }
    project_skill_dir
}

fn get_project_skill_dir(working_directory: &Path, skill_name: &str) -> PathBuf {
    let plural = working_directory
        .join(".opencode")
        .join("skills")
        .join(skill_name);
    let legacy = working_directory
        .join(".opencode")
        .join("skill")
        .join(skill_name);
    if legacy.exists() && !plural.exists() {
        return legacy;
    }
    plural
}

fn get_project_skill_path(working_directory: &Path, skill_name: &str) -> PathBuf {
    get_project_skill_dir(working_directory, skill_name).join("SKILL.md")
}

fn get_user_skill_dir(home: &Path, skill_name: &str) -> PathBuf {
    let plural = skill_dir(home).join(skill_name);
    let legacy = opencode_config_dir(home).join("skill").join(skill_name);
    if legacy.exists() && !plural.exists() {
        return legacy;
    }
    plural
}

fn get_user_skill_path(home: &Path, skill_name: &str) -> PathBuf {
    get_user_skill_dir(home, skill_name).join("SKILL.md")
}

fn get_claude_skill_dir(working_directory: &Path, skill_name: &str) -> PathBuf {
    working_directory
        .join(".claude")
        .join("skills")
        .join(skill_name)
}

fn get_claude_skill_path(working_directory: &Path, skill_name: &str) -> PathBuf {
    get_claude_skill_dir(working_directory, skill_name).join("SKILL.md")
}

fn get_user_claude_skill_path(home: &Path, skill_name: &str) -> PathBuf {
    home.join(".claude")
        .join("skills")
        .join(skill_name)
        .join("SKILL.md")
}

fn get_user_agents_skill_dir(home: &Path, skill_name: &str) -> PathBuf {
    home.join(".agents").join("skills").join(skill_name)
}

fn get_user_agents_skill_path(home: &Path, skill_name: &str) -> PathBuf {
    get_user_agents_skill_dir(home, skill_name).join("SKILL.md")
}

fn get_project_agents_skill_dir(working_directory: &Path, skill_name: &str) -> PathBuf {
    working_directory
        .join(".agents")
        .join("skills")
        .join(skill_name)
}

fn get_project_agents_skill_path(working_directory: &Path, skill_name: &str) -> PathBuf {
    get_project_agents_skill_dir(working_directory, skill_name).join("SKILL.md")
}

/// A discovered skill row (the JSON wire shape of the list/get routes).
#[derive(Debug, Clone, Default)]
pub(crate) struct DiscoveredSkill {
    pub name: String,
    pub path: Option<String>,
    pub scope: Option<String>,
    pub source: Option<String>,
    pub description: Option<String>,
    pub content: Option<String>,
}

impl DiscoveredSkill {
    pub(crate) fn to_value(&self) -> Value {
        let mut map = Map::new();
        map.insert("name".into(), Value::from(self.name.clone()));
        map.insert(
            "path".into(),
            self.path.clone().map(Value::from).unwrap_or(Value::Null),
        );
        map.insert(
            "scope".into(),
            self.scope.clone().map(Value::from).unwrap_or(Value::Null),
        );
        map.insert(
            "source".into(),
            self.source.clone().map(Value::from).unwrap_or(Value::Null),
        );
        map.insert(
            "description".into(),
            self.description
                .clone()
                .map(Value::from)
                .unwrap_or(Value::Null),
        );
        if let Some(content) = &self.content {
            map.insert("content".into(), Value::from(content.clone()));
        }
        Value::Object(map)
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SkillScope {
    pub scope: Option<String>,
    pub path: Option<String>,
    pub source: Option<String>,
}

pub(crate) fn get_skill_scope(
    skill_name: &str,
    working_directory: Option<&Path>,
    home: &Path,
) -> SkillScope {
    let discovered = discover_skills(working_directory, home)
        .into_iter()
        .find(|skill| skill.name == skill_name);
    if let Some(discovered) = discovered
        && discovered.path.is_some()
    {
        return SkillScope {
            scope: discovered.scope.filter(|scope| !scope.is_empty()),
            path: discovered.path.clone(),
            source: discovered.source.filter(|source| !source.is_empty()),
        };
    }

    if let Some(working_directory) = working_directory {
        let project_path = get_project_skill_path(working_directory, skill_name);
        if project_path.exists() {
            return SkillScope {
                scope: Some(SKILL_SCOPE_PROJECT.into()),
                path: Some(project_path.to_string_lossy().into_owned()),
                source: Some("opencode".into()),
            };
        }
        let claude_path = get_claude_skill_path(working_directory, skill_name);
        if claude_path.exists() {
            return SkillScope {
                scope: Some(SKILL_SCOPE_PROJECT.into()),
                path: Some(claude_path.to_string_lossy().into_owned()),
                source: Some("claude".into()),
            };
        }
    }

    let user_path = get_user_skill_path(home, skill_name);
    if user_path.exists() {
        return SkillScope {
            scope: Some(SKILL_SCOPE_USER.into()),
            path: Some(user_path.to_string_lossy().into_owned()),
            source: Some("opencode".into()),
        };
    }
    let user_claude_path = get_user_claude_skill_path(home, skill_name);
    if user_claude_path.exists() {
        return SkillScope {
            scope: Some(SKILL_SCOPE_USER.into()),
            path: Some(user_claude_path.to_string_lossy().into_owned()),
            source: Some("claude".into()),
        };
    }
    let user_agents_path = get_user_agents_skill_path(home, skill_name);
    if user_agents_path.exists() {
        return SkillScope {
            scope: Some(SKILL_SCOPE_USER.into()),
            path: Some(user_agents_path.to_string_lossy().into_owned()),
            source: Some("agents".into()),
        };
    }
    SkillScope::default()
}

/// `walkSkillMdFiles` — every `SKILL.md` under `root_dir`, recursively.
pub(crate) fn walk_skill_md_files(root_dir: &Path) -> Vec<PathBuf> {
    let mut results = Vec::new();
    walk_dir_for_skill_md(root_dir, &mut results);
    results
}

fn walk_dir_for_skill_md(dir: &Path, results: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let full_path = dir.join(entry.file_name());
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            walk_dir_for_skill_md(&full_path, results);
        } else if file_type.is_file() && entry.file_name() == "SKILL.md" {
            results.push(full_path);
        }
    }
}

fn add_skill_from_md_file(
    skills: &mut BTreeMap<String, DiscoveredSkill>,
    skill_md_path: &Path,
    scope: &str,
    source: &str,
) {
    let Ok(raw) = std::fs::read_to_string(skill_md_path) else {
        return;
    };
    let (frontmatter, _) = parse_md_content(&raw);
    let Some(name) = frontmatter.get("name").and_then(Value::as_str) else {
        return;
    };
    let name = name.trim().to_string();
    if name.is_empty() {
        return;
    }
    let description = frontmatter
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default();
    skills.insert(
        name.clone(),
        DiscoveredSkill {
            name,
            path: Some(skill_md_path.to_string_lossy().into_owned()),
            scope: Some(scope.to_string()),
            source: Some(source.to_string()),
            description: Some(description.to_string()),
            content: None,
        },
    );
}

/// `resolveSkillSearchDirectories` (shared.js).
fn resolve_skill_search_directories(working_directory: Option<&Path>, home: &Path) -> Vec<PathBuf> {
    let mut directories: Vec<PathBuf> = Vec::new();
    let mut push_dir = |dir: Option<PathBuf>| {
        let Some(dir) = dir else {
            return;
        };
        let resolved = super::http_util::resolve_path(&dir.to_string_lossy());
        if !directories.contains(&resolved) {
            directories.push(resolved);
        }
    };
    push_dir(Some(opencode_config_dir(home)));
    if let Some(working_directory) = working_directory {
        let worktree_root = find_worktree_root(working_directory).unwrap_or_else(|| {
            super::http_util::resolve_path(&working_directory.to_string_lossy())
        });
        for ancestor in get_ancestors(working_directory, Some(&worktree_root)) {
            push_dir(Some(ancestor.join(".opencode")));
        }
    }
    push_dir(Some(home.join(".opencode")));
    if let Ok(custom) = std::env::var("OPENCODE_CONFIG_DIR") {
        push_dir(Some(PathBuf::from(custom)));
    }
    directories
}

/// `readConfig` (merged user/project/custom layers) reduced to the value
/// tree; INVALID_JSONC layers degrade to `{}` like the JS.
fn read_merged_config(working_directory: Option<&Path>, home: &Path) -> Value {
    let user_paths = [
        opencode_config_dir(home).join("config.json"),
        opencode_config_dir(home).join("opencode.json"),
        opencode_config_dir(home).join("opencode.jsonc"),
    ];
    let user_path = user_paths
        .iter()
        .find(|p| p.exists())
        .cloned()
        .unwrap_or_else(|| user_paths[0].clone());
    let project_path = working_directory.and_then(|dir| {
        [
            dir.join("opencode.json"),
            dir.join("opencode.jsonc"),
            dir.join(".opencode").join("opencode.json"),
            dir.join(".opencode").join("opencode.jsonc"),
        ]
        .into_iter()
        .find(|p| p.exists())
    });
    let custom_path = std::env::var("OPENCODE_CONFIG")
        .ok()
        .map(|v| super::http_util::resolve_path(&v));

    let mut merged = Map::new();
    for path in [Some(user_path), project_path, custom_path]
        .into_iter()
        .flatten()
    {
        if let Ok(layer) = read_config_layer(Some(&path)) {
            merge_config_maps(&mut merged, &layer.config);
        }
    }
    Value::Object(merged)
}

fn merge_config_maps(base: &mut Map<String, Value>, overlay: &Map<String, Value>) {
    for (key, value) in overlay {
        match (base.get(key), value) {
            (Some(Value::Object(base_inner)), Value::Object(overlay_inner)) => {
                let mut base_inner = base_inner.clone();
                merge_config_maps(&mut base_inner, overlay_inner);
                base.insert(key.clone(), Value::Object(base_inner));
            }
            _ => {
                base.insert(key.clone(), value.clone());
            }
        }
    }
}

/// `discoverSkills` — home external roots, project ancestors' external
/// roots, OpenCode config dirs (`skill`/`skills`), configured `skills.paths`,
/// then the OpenCode cache roots. Later discoveries replace earlier ones by
/// name.
pub(crate) fn discover_skills(
    working_directory: Option<&Path>,
    home: &Path,
) -> Vec<DiscoveredSkill> {
    let mut skills: BTreeMap<String, DiscoveredSkill> = BTreeMap::new();

    for external_root_name in [".claude", ".agents"] {
        let source = if external_root_name == ".agents" {
            "agents"
        } else {
            "claude"
        };
        let home_root = home.join(external_root_name).join("skills");
        for skill_md_path in walk_skill_md_files(&home_root) {
            add_skill_from_md_file(&mut skills, &skill_md_path, SKILL_SCOPE_USER, source);
        }
    }

    if let Some(working_directory) = working_directory {
        let worktree_root = find_worktree_root(working_directory).unwrap_or_else(|| {
            super::http_util::resolve_path(&working_directory.to_string_lossy())
        });
        for ancestor in get_ancestors(working_directory, Some(&worktree_root)) {
            for external_root_name in [".claude", ".agents"] {
                let source = if external_root_name == ".agents" {
                    "agents"
                } else {
                    "claude"
                };
                let root = ancestor.join(external_root_name).join("skills");
                for skill_md_path in walk_skill_md_files(&root) {
                    add_skill_from_md_file(
                        &mut skills,
                        &skill_md_path,
                        SKILL_SCOPE_PROJECT,
                        source,
                    );
                }
            }
        }
    }

    let home_opencode_dir =
        super::http_util::resolve_path(&home.join(".opencode").to_string_lossy());
    let custom_config_dir = std::env::var("OPENCODE_CONFIG_DIR")
        .ok()
        .map(|v| super::http_util::resolve_path(&v));
    for dir in resolve_skill_search_directories(working_directory, home) {
        for sub_dir in ["skill", "skills"] {
            let root = dir.join(sub_dir);
            for skill_md_path in walk_skill_md_files(&root) {
                let is_user_config_dir = dir == opencode_config_dir(home)
                    || dir == home_opencode_dir
                    || custom_config_dir.as_ref() == Some(&dir);
                let scope = if is_user_config_dir {
                    SKILL_SCOPE_USER
                } else {
                    SKILL_SCOPE_PROJECT
                };
                add_skill_from_md_file(&mut skills, &skill_md_path, scope, "opencode");
            }
        }
    }

    let configured_paths: Vec<String> = read_merged_config(working_directory, home)
        .get("skills")
        .and_then(|skills| skills.get("paths"))
        .and_then(Value::as_array)
        .map(|paths| {
            paths
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    for skill_path in configured_paths {
        if skill_path.trim().is_empty() {
            continue;
        }
        let expanded = if let Some(rest) = skill_path.strip_prefix("~/") {
            home.join(rest).to_string_lossy().into_owned()
        } else {
            skill_path.clone()
        };
        let resolved = if Path::new(&expanded).is_absolute() {
            super::http_util::resolve_path(&expanded)
        } else {
            let base = working_directory
                .map(|dir| dir.to_path_buf())
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));
            super::http_util::resolve_path(&base.join(&expanded).to_string_lossy())
        };
        for skill_md_path in walk_skill_md_files(&resolved) {
            add_skill_from_md_file(&mut skills, &skill_md_path, SKILL_SCOPE_PROJECT, "opencode");
        }
    }

    let mut cache_candidates = Vec::new();
    if let Ok(xdg_cache) = std::env::var("XDG_CACHE_HOME") {
        cache_candidates.push(PathBuf::from(xdg_cache).join("opencode").join("skills"));
    }
    cache_candidates.push(home.join(".cache").join("opencode").join("skills"));
    cache_candidates.push(
        home.join("Library")
            .join("Caches")
            .join("opencode")
            .join("skills"),
    );

    for cache_root in cache_candidates {
        let Ok(entries) = std::fs::read_dir(&cache_root) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let skill_root = cache_root.join(entry.file_name());
            for skill_md_path in walk_skill_md_files(&skill_root) {
                add_skill_from_md_file(&mut skills, &skill_md_path, SKILL_SCOPE_USER, "opencode");
            }
        }
    }

    skills.into_values().collect()
}

/// `mergeDiscoveredSkills` — primary first, dedupe by trimmed name.
pub(crate) fn merge_discovered_skills(
    primary: &[DiscoveredSkill],
    fallback: &[DiscoveredSkill],
) -> Vec<DiscoveredSkill> {
    let mut merged = Vec::new();
    let mut seen_names = Vec::new();
    let mut append = |skill: &DiscoveredSkill| {
        let name = skill.name.trim().to_string();
        if name.is_empty() || seen_names.contains(&name) {
            return;
        }
        seen_names.push(name);
        merged.push(skill.clone());
    };
    for skill in primary {
        append(skill);
    }
    for skill in fallback {
        append(skill);
    }
    merged
}

/// `listSkillSupportingFiles` — everything except SKILL.md, recursively.
pub(crate) fn list_skill_supporting_files(skill_dir: &Path) -> Vec<Value> {
    let mut files = Vec::new();
    walk_supporting_files(skill_dir, "", &mut files);
    files
}

fn walk_supporting_files(dir: &Path, relative_path: &str, files: &mut Vec<Value>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut sorted: Vec<_> = entries.flatten().collect();
    sorted.sort_by_key(|entry| entry.file_name());
    for entry in sorted {
        let name = entry.file_name().to_string_lossy().into_owned();
        let full_path = dir.join(&name);
        let rel_path = if relative_path.is_empty() {
            name.clone()
        } else {
            format!("{relative_path}/{name}")
        };
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            walk_supporting_files(&full_path, &rel_path, files);
        } else if name != "SKILL.md" {
            files.push(json!({
                "name": name,
                "path": rel_path,
                "fullPath": full_path.to_string_lossy().as_ref(),
            }));
        }
    }
}

/// `assertPathWithinSkillDir` — realpath containment; violations are EACCES
/// (`Access to file denied`).
fn assert_path_within_skill_dir(
    skill_dir: &Path,
    relative_path: &str,
) -> Result<PathBuf, AccessError> {
    let root = std::fs::canonicalize(skill_dir).map_err(|_| AccessError)?;
    let target = super::http_util::resolve_path(&root.join(relative_path).to_string_lossy());
    let root_display = root.to_string_lossy().into_owned();
    let target_display = target.to_string_lossy().into_owned();
    let within = target_display == root_display
        || (target_display.starts_with(&root_display)
            && target_display[root_display.len()..].starts_with(std::path::MAIN_SEPARATOR));
    if !within {
        return Err(AccessError);
    }
    Ok(target)
}

#[derive(Debug)]
pub(crate) struct AccessError;

pub(crate) fn read_skill_supporting_file(
    skill_dir: &Path,
    relative_path: &str,
) -> Result<Option<String>, AccessError> {
    let full_path = assert_path_within_skill_dir(skill_dir, relative_path)?;
    if !full_path.exists() {
        return Ok(None);
    }
    std::fs::read_to_string(&full_path)
        .map(Some)
        .map_err(|_| AccessError)
}

pub(crate) fn write_skill_supporting_file(
    skill_dir: &Path,
    relative_path: &str,
    content: &str,
) -> Result<(), AccessError> {
    let full_path = assert_path_within_skill_dir(skill_dir, relative_path)?;
    if let Some(parent) = full_path.parent() {
        std::fs::create_dir_all(parent).map_err(|_| AccessError)?;
    }
    std::fs::write(&full_path, content).map_err(|_| AccessError)
}

pub(crate) fn delete_skill_supporting_file(
    skill_dir: &Path,
    relative_path: &str,
) -> Result<(), AccessError> {
    let root = std::fs::canonicalize(skill_dir).map_err(|_| AccessError)?;
    let full_path = assert_path_within_skill_dir(skill_dir, relative_path)?;
    if full_path.exists() {
        std::fs::remove_file(&full_path).map_err(|_| AccessError)?;
        let mut parent_dir = full_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| root.clone());
        while parent_dir != root {
            match std::fs::read_dir(&parent_dir) {
                Ok(mut entries) => {
                    if entries.next().is_some() {
                        break;
                    }
                    std::fs::remove_dir(&parent_dir).ok();
                }
                Err(_) => break,
            }
            parent_dir = parent_dir
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| root.clone());
        }
    }
    Ok(())
}

fn is_readable_file(path: Option<&str>) -> bool {
    path.is_some_and(|path| std::fs::metadata(path).is_ok_and(|m| m.is_file()))
}

/// `getSkillSources` — the per-skill source map used by the skill routes.
/// I/O failures reading the resolved SKILL.md surface as errors (the JS lets
/// the stat/read throw to the route's 500).
pub(crate) fn get_skill_sources(
    skill_name: &str,
    working_directory: Option<&Path>,
    discovered_skill: Option<&DiscoveredSkill>,
    home: &Path,
) -> Result<Value, std::io::Error> {
    let working_dir = working_directory.map(PathBuf::from);

    let project_path = working_dir
        .as_ref()
        .map(|dir| get_project_skill_path(dir, skill_name));
    let project_exists = project_path.as_ref().is_some_and(|p| p.exists());
    let project_dir = || {
        project_path
            .as_ref()
            .and_then(|p| p.parent().map(Path::to_path_buf))
    };

    let claude_path = working_dir
        .as_ref()
        .map(|dir| get_claude_skill_path(dir, skill_name));
    let claude_exists = claude_path.as_ref().is_some_and(|p| p.exists());
    let claude_dir = || {
        claude_path
            .as_ref()
            .and_then(|p| p.parent().map(Path::to_path_buf))
    };

    let user_claude_path = get_user_claude_skill_path(home, skill_name);
    let user_claude_exists = user_claude_path.exists();
    let user_claude_dir = || user_claude_path.parent().map(Path::to_path_buf);

    let user_path = get_user_skill_path(home, skill_name);
    let user_exists = user_path.exists();
    let user_dir = || user_path.parent().map(Path::to_path_buf);

    let user_agents_path = get_user_agents_skill_path(home, skill_name);
    let user_agents_exists = user_agents_path.exists();
    let user_agents_dir = || user_agents_path.parent().map(Path::to_path_buf);

    let matched_discovered = match discovered_skill {
        Some(skill) if skill.name == skill_name => Some(skill.clone()),
        _ => discover_skills(working_directory, home)
            .into_iter()
            .find(|skill| skill.name == skill_name),
    };
    let discovered_description = matched_discovered
        .as_ref()
        .and_then(|skill| skill.description.clone())
        .unwrap_or_default();
    let discovered_content = matched_discovered
        .as_ref()
        .and_then(|skill| skill.content.clone())
        .unwrap_or_default();
    let discovered_path = matched_discovered
        .as_ref()
        .and_then(|skill| skill.path.clone());
    let is_built_in_discovered = discovered_path.as_deref() == Some(BUILT_IN_SKILL_LOCATION);

    let mut md_path: Option<String> = None;
    let mut md_scope: Option<String> = None;
    let mut md_source: Option<String> = None;
    let mut md_dir: Option<PathBuf> = None;

    if is_built_in_discovered {
        md_scope = Some(
            matched_discovered
                .as_ref()
                .and_then(|s| s.scope.clone())
                .unwrap_or_else(|| SKILL_SCOPE_USER.into()),
        );
        md_source = Some(
            matched_discovered
                .as_ref()
                .and_then(|s| s.source.clone())
                .unwrap_or_else(|| "opencode".into()),
        );
    } else if let Some(discovered_path) = &discovered_path {
        md_path = Some(discovered_path.clone());
        md_scope = matched_discovered.as_ref().and_then(|s| s.scope.clone());
        md_source = matched_discovered.as_ref().and_then(|s| s.source.clone());
        md_dir = is_readable_file(Some(discovered_path))
            .then(|| Path::new(discovered_path).parent().map(Path::to_path_buf))
            .flatten();
    } else if project_exists {
        md_path = project_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned());
        md_scope = Some(SKILL_SCOPE_PROJECT.into());
        md_source = Some("opencode".into());
        md_dir = project_dir();
    } else if claude_exists {
        md_path = claude_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned());
        md_scope = Some(SKILL_SCOPE_PROJECT.into());
        md_source = Some("claude".into());
        md_dir = claude_dir();
    } else if user_exists {
        md_path = Some(user_path.to_string_lossy().into_owned());
        md_scope = Some(SKILL_SCOPE_USER.into());
        md_source = Some("opencode".into());
        md_dir = user_dir();
    } else if user_claude_exists {
        md_path = Some(user_claude_path.to_string_lossy().into_owned());
        md_scope = Some(SKILL_SCOPE_USER.into());
        md_source = Some("claude".into());
        md_dir = user_claude_dir();
    } else if user_agents_exists {
        md_path = Some(user_agents_path.to_string_lossy().into_owned());
        md_scope = Some(SKILL_SCOPE_USER.into());
        md_source = Some("agents".into());
        md_dir = user_agents_dir();
    }

    let md_exists = is_built_in_discovered || is_readable_file(md_path.as_deref());
    if !md_exists {
        md_path = None;
        md_dir = None;
        md_scope = None;
        md_source = None;
    }

    let mut md = Map::new();
    md.insert("exists".into(), Value::Bool(md_exists));
    md.insert(
        "path".into(),
        md_path.clone().map(Value::from).unwrap_or(Value::Null),
    );
    md.insert(
        "dir".into(),
        md_dir
            .as_ref()
            .map(|d| Value::from(d.to_string_lossy().as_ref()))
            .unwrap_or(Value::Null),
    );
    md.insert(
        "scope".into(),
        md_scope.clone().map(Value::from).unwrap_or(Value::Null),
    );
    md.insert(
        "source".into(),
        md_source.clone().map(Value::from).unwrap_or(Value::Null),
    );
    md.insert(
        "fields".into(),
        Value::Array(if is_built_in_discovered {
            vec![Value::from("description"), Value::from("instructions")]
        } else {
            Vec::new()
        }),
    );
    md.insert("supportingFiles".into(), Value::Array(Vec::new()));
    md.insert(
        "name".into(),
        Value::from(
            matched_discovered
                .as_ref()
                .map(|s| s.name.clone())
                .unwrap_or_else(|| skill_name.to_string()),
        ),
    );
    md.insert(
        "description".into(),
        Value::from(discovered_description.clone()),
    );
    md.insert(
        "instructions".into(),
        Value::from(if is_built_in_discovered {
            discovered_content.clone()
        } else {
            String::new()
        }),
    );

    if md_exists && let Some(md_dir) = &md_dir {
        let md_path = md_path.as_deref().unwrap_or_default();
        let raw = std::fs::read_to_string(md_path)?;
        let (frontmatter, body) = parse_md_content(&raw);
        let fields: Vec<Value> = frontmatter
            .keys()
            .map(|key| Value::from(key.as_str()))
            .collect();
        md.insert("fields".into(), Value::Array(fields));
        md.insert(
            "description".into(),
            Value::from(
                frontmatter
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
            ),
        );
        md.insert(
            "name".into(),
            Value::from(
                frontmatter
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(skill_name),
            ),
        );
        if !body.is_empty() {
            let mut fields = md
                .get("fields")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            fields.push(Value::from("instructions"));
            md.insert("fields".into(), Value::Array(fields));
            md.insert("instructions".into(), Value::from(body));
        } else {
            md.insert("instructions".into(), Value::from(""));
        }
        md.insert(
            "supportingFiles".into(),
            Value::Array(list_skill_supporting_files(md_dir)),
        );
    }

    let source_entry = |exists: bool, path: Option<PathBuf>| {
        Value::Object(Map::from_iter([
            ("exists".into(), Value::Bool(exists)),
            (
                "path".into(),
                path.as_ref()
                    .map(|p| Value::from(p.to_string_lossy().as_ref()))
                    .unwrap_or(Value::Null),
            ),
            (
                "dir".into(),
                path.as_ref()
                    .and_then(|p| p.parent())
                    .map(|p| Value::from(p.to_string_lossy().as_ref()))
                    .unwrap_or(Value::Null),
            ),
        ]))
    };

    Ok(Value::Object(Map::from_iter([
        ("md".into(), Value::Object(md)),
        (
            "projectMd".into(),
            source_entry(project_exists, project_path),
        ),
        ("claudeMd".into(), source_entry(claude_exists, claude_path)),
        ("userMd".into(), source_entry(user_exists, Some(user_path))),
        (
            "userClaudeMd".into(),
            source_entry(user_claude_exists, Some(user_claude_path)),
        ),
        (
            "userAgentsMd".into(),
            source_entry(user_agents_exists, Some(user_agents_path)),
        ),
    ])))
}

/// `/^[a-z0-9][a-z0-9-]*[a-z0-9]$|^[a-z0-9]$/` with length 1-64.
pub(crate) fn is_valid_skill_name(skill_name: &str) -> bool {
    let count = skill_name.chars().count();
    if count == 0 || count > 64 {
        return false;
    }
    let bytes = skill_name.as_bytes();
    let first_last_ok = bytes
        .first()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && bytes
            .last()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    if count == 1 {
        return first_last_ok;
    }
    first_last_ok
        && bytes[1..count - 1]
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

fn assert_valid_skill_name(skill_name: &str) -> Result<(), String> {
    if !is_valid_skill_name(skill_name) {
        return Err(format!(
            "Invalid skill name \"{skill_name}\". Must be 1-64 lowercase alphanumeric characters with hyphens, cannot start or end with hyphen."
        ));
    }
    Ok(())
}

/// `createSkill`.
pub(crate) fn create_skill(
    skill_name: &str,
    config: &Value,
    working_directory: Option<&Path>,
    scope: Option<&str>,
    home: &Path,
) -> Result<(), String> {
    ensure_dirs(home);
    assert_valid_skill_name(skill_name)?;

    let existing = get_skill_scope(skill_name, working_directory, home);
    if let Some(path) = existing.path {
        return Err(format!("Skill {skill_name} already exists at {path}"));
    }

    let requested_scope = if scope == Some(SKILL_SCOPE_PROJECT) {
        SKILL_SCOPE_PROJECT
    } else {
        SKILL_SCOPE_USER
    };
    let requested_source = if config.get("source").and_then(Value::as_str) == Some("agents") {
        "agents"
    } else {
        "opencode"
    };

    let (target_dir, target_path) =
        if requested_scope == SKILL_SCOPE_PROJECT && working_directory.is_some() {
            let working_directory = working_directory.expect("checked above");
            ensure_project_skill_dir(working_directory);
            if requested_source == "agents" {
                (
                    get_project_agents_skill_dir(working_directory, skill_name),
                    get_project_agents_skill_path(working_directory, skill_name),
                )
            } else {
                (
                    get_project_skill_dir(working_directory, skill_name),
                    get_project_skill_path(working_directory, skill_name),
                )
            }
        } else if requested_source == "agents" {
            (
                get_user_agents_skill_dir(home, skill_name),
                get_user_agents_skill_path(home, skill_name),
            )
        } else {
            (
                get_user_skill_dir(home, skill_name),
                get_user_skill_path(home, skill_name),
            )
        };

    std::fs::create_dir_all(&target_dir).map_err(|error| error.to_string())?;

    let mut frontmatter: Map<String, Value> = config.as_object().cloned().unwrap_or_default();
    for key in ["instructions", "scope", "source", "supportingFiles"] {
        frontmatter.remove(key);
    }
    frontmatter
        .entry("name".to_string())
        .or_insert_with(|| Value::from(skill_name));
    if matches!(frontmatter.get("description"), None | Some(Value::Null))
        || frontmatter.get("description").and_then(Value::as_str) == Some("")
    {
        return Err("Skill description is required".to_string());
    }
    let instructions = config
        .get("instructions")
        .and_then(Value::as_str)
        .unwrap_or("");
    write_md_file(&target_path, &frontmatter, instructions)
        .map_err(|_| "Failed to write agent markdown file".to_string())?;

    if let Some(files) = config.get("supportingFiles").and_then(Value::as_array) {
        for file in files {
            if let Some(path) = file.get("path").and_then(Value::as_str)
                && file.get("content").is_some()
            {
                let content = file.get("content").and_then(Value::as_str).unwrap_or("");
                write_skill_supporting_file(&target_dir, path, content)
                    .map_err(|_| error_message_for_support_write())?;
            }
        }
    }

    tracing::info!(
        "Created new skill: {skill_name} (scope: {requested_scope}, path: {})",
        target_path.display()
    );
    Ok(())
}

fn error_message_for_support_write() -> String {
    "Failed to write skill supporting file".to_string()
}

/// `updateSkill`.
pub(crate) fn update_skill(
    skill_name: &str,
    updates: &Value,
    working_directory: Option<&Path>,
    target_path: Option<&str>,
    home: &Path,
) -> Result<(), String> {
    ensure_dirs(home);

    let requested_path = target_path
        .filter(|p| !p.trim().is_empty())
        .map(|p| super::http_util::resolve_path(p.trim()));
    let existing_scope = if requested_path.as_ref().is_some_and(|p| p.exists()) {
        SkillScope {
            scope: None,
            path: requested_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            source: None,
        }
    } else {
        get_skill_scope(skill_name, working_directory, home)
    };
    let Some(existing_path) = existing_scope.path.clone() else {
        return Err(format!("Skill \"{skill_name}\" not found"));
    };
    if Path::new(&existing_path)
        .file_name()
        .map(|n| n != "SKILL.md")
        .unwrap_or(true)
    {
        return Err(format!(
            "Skill \"{skill_name}\" target must be a SKILL.md file"
        ));
    }

    let md_path = PathBuf::from(&existing_path);
    let md_dir = md_path.parent().map(Path::to_path_buf).unwrap_or_default();
    let raw = std::fs::read_to_string(&md_path).map_err(|error| error.to_string())?;
    let (mut frontmatter, mut body) = parse_md_content(&raw);
    let frontmatter_name = frontmatter
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| skill_name.to_string());
    if frontmatter_name != skill_name {
        return Err(format!(
            "Skill \"{skill_name}\" does not match {existing_path}"
        ));
    }

    let mut md_modified = false;
    if let Some(updates) = updates.as_object() {
        for (field, value) in updates {
            if matches!(
                field.as_str(),
                "scope" | "source" | "targetPath" | "renameTo"
            ) {
                continue;
            }
            if field == "instructions" {
                body = match value {
                    Value::Null => String::new(),
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                };
                md_modified = true;
                continue;
            }
            if field == "supportingFiles" {
                if let Some(files) = value.as_array() {
                    for file in files {
                        if matches!(file.get("delete"), Some(v) if !matches!(v, Value::Null | Value::Bool(false)))
                            && file.get("path").and_then(Value::as_str).is_some()
                        {
                            delete_skill_supporting_file(
                                &md_dir,
                                file.get("path").and_then(Value::as_str).unwrap_or_default(),
                            )
                            .map_err(|_| "Access to file denied".to_string())?;
                        } else if let Some(path) = file.get("path").and_then(Value::as_str)
                            && file.get("content").is_some()
                        {
                            let content = file.get("content").and_then(Value::as_str).unwrap_or("");
                            write_skill_supporting_file(&md_dir, path, content)
                                .map_err(|_| "Access to file denied".to_string())?;
                        }
                    }
                }
                continue;
            }
            frontmatter.insert(field.clone(), value.clone());
            md_modified = true;
        }
    }

    if md_modified {
        write_md_file(&md_path, &frontmatter, &body)
            .map_err(|_| "Failed to write agent markdown file".to_string())?;
    }
    tracing::info!("Updated skill: {skill_name} (path: {existing_path})");
    Ok(())
}

/// `deleteSkill` — removes every on-disk copy; nothing found is an error.
pub(crate) fn delete_skill(
    skill_name: &str,
    working_directory: Option<&Path>,
    home: &Path,
) -> Result<(), String> {
    let mut deleted = false;
    let remove_dir = |dir: &Path, label: &str| -> Result<bool, String> {
        if !dir.exists() {
            return Ok(false);
        }
        std::fs::remove_dir_all(dir).map_err(|error| error.to_string())?;
        tracing::info!("Deleted {label} skill directory: {}", dir.display());
        Ok(true)
    };

    if let Some(working_directory) = working_directory {
        deleted |= remove_dir(
            &get_project_skill_dir(working_directory, skill_name),
            "project-level",
        )?;
        deleted |= remove_dir(
            &get_claude_skill_dir(working_directory, skill_name),
            "claude-compat",
        )?;
        deleted |= remove_dir(
            &get_project_agents_skill_dir(working_directory, skill_name),
            "project-level agents",
        )?;
    }
    deleted |= remove_dir(&get_user_skill_dir(home, skill_name), "user-level")?;
    deleted |= remove_dir(
        &get_user_agents_skill_dir(home, skill_name),
        "user-level agents",
    )?;
    let user_claude_dir = get_user_claude_skill_path(home, skill_name)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    deleted |= remove_dir(&user_claude_dir, "user-level claude")?;

    if !deleted {
        return Err(format!("Skill \"{skill_name}\" not found"));
    }
    Ok(())
}

/// `isPathInside` — resolved equality or child-of.
pub(crate) fn is_path_inside(candidate_path: &str, parent_path: &str) -> bool {
    if candidate_path.is_empty() || parent_path.is_empty() {
        return false;
    }
    let candidate = super::http_util::resolve_path(candidate_path);
    let parent = super::http_util::resolve_path(parent_path);
    let candidate_display = candidate.to_string_lossy();
    let parent_display = parent.to_string_lossy();
    candidate_display == parent_display
        || (candidate_display.starts_with(parent_display.as_ref())
            && candidate_display
                .get(parent_display.len()..)
                .is_some_and(|rest| rest.starts_with(std::path::MAIN_SEPARATOR)))
}

/// `getManagedSkillRoots`.
fn managed_skill_roots(working_directory: Option<&Path>, home: &Path) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut push_root = |dir: PathBuf| {
        let resolved = super::http_util::resolve_path(&dir.to_string_lossy());
        if !roots.contains(&resolved) {
            roots.push(resolved);
        }
    };

    push_root(skill_dir(home));
    push_root(opencode_config_dir(home).join("skill"));
    push_root(home.join(".opencode").join("skills"));
    push_root(home.join(".opencode").join("skill"));
    push_root(home.join(".claude").join("skills"));
    push_root(home.join(".agents").join("skills"));

    if let Ok(custom) = std::env::var("OPENCODE_CONFIG_DIR") {
        let custom_dir = super::http_util::resolve_path(&custom);
        push_root(custom_dir.join("skills"));
        push_root(custom_dir.join("skill"));
    }

    if let Some(working_directory) = working_directory {
        let worktree_root = find_worktree_root(working_directory).unwrap_or_else(|| {
            super::http_util::resolve_path(&working_directory.to_string_lossy())
        });
        for ancestor in get_ancestors(working_directory, Some(&worktree_root)) {
            push_root(ancestor.join(".opencode").join("skills"));
            push_root(ancestor.join(".opencode").join("skill"));
            push_root(ancestor.join(".claude").join("skills"));
            push_root(ancestor.join(".agents").join("skills"));
        }
    }

    roots
}

/// `isManagedSkillPath`.
pub(crate) fn is_managed_skill_path(
    skill_md_path: &str,
    working_directory: Option<&Path>,
    home: &Path,
) -> bool {
    if skill_md_path.is_empty() || skill_md_path == BUILT_IN_SKILL_LOCATION {
        return false;
    }
    let resolved = super::http_util::resolve_path(skill_md_path);
    let skill_dir = resolved.parent().map(Path::to_path_buf).unwrap_or_default();
    let skill_dir_display = skill_dir.to_string_lossy().into_owned();
    let canonical_display = std::fs::canonicalize(&skill_dir)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| skill_dir_display.clone());
    managed_skill_roots(working_directory, home)
        .iter()
        .any(|root| {
            let root_display = root.to_string_lossy();
            is_path_inside(&skill_dir_display, &root_display)
                || is_path_inside(&canonical_display, &root_display)
        })
}

/// `renameSkill` — directory rename preserving body + supporting files, with
/// rollback when the frontmatter rewrite fails.
pub(crate) fn rename_skill(
    old_name: &str,
    new_name: &str,
    working_directory: Option<&Path>,
    home: &Path,
) -> Result<(), String> {
    ensure_dirs(home);
    assert_valid_skill_name(new_name)?;

    if old_name == new_name {
        return Ok(());
    }

    let existing = get_skill_scope(old_name, working_directory, home);
    let Some(existing_path) = existing.path else {
        return Err(format!("Skill \"{old_name}\" not found"));
    };
    if existing_path == BUILT_IN_SKILL_LOCATION || !Path::new(&existing_path).exists() {
        return Err(format!("Skill \"{old_name}\" cannot be renamed"));
    }
    if Path::new(&existing_path)
        .file_name()
        .map(|n| n != "SKILL.md")
        .unwrap_or(true)
    {
        return Err(format!(
            "Skill \"{old_name}\" target must be a SKILL.md file"
        ));
    }
    if !is_managed_skill_path(&existing_path, working_directory, home) {
        return Err(format!(
            "Skill \"{old_name}\" is outside managed skill directories and cannot be renamed"
        ));
    }

    let old_path = PathBuf::from(&existing_path);
    let raw = std::fs::read_to_string(&old_path).map_err(|error| error.to_string())?;
    let (frontmatter, _) = parse_md_content(&raw);
    let frontmatter_name = frontmatter
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| old_name.to_string());
    if frontmatter_name != old_name {
        return Err(format!(
            "Skill \"{old_name}\" does not match {existing_path}"
        ));
    }

    let conflict = get_skill_scope(new_name, working_directory, home);
    if let Some(conflict_path) = conflict.path {
        return Err(format!(
            "Skill {new_name} already exists at {conflict_path}"
        ));
    }

    let old_dir = old_path.parent().map(Path::to_path_buf).unwrap_or_default();
    let new_dir = old_dir
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
        .join(new_name);
    let directories_differ = super::http_util::resolve_path(&old_dir.to_string_lossy())
        != super::http_util::resolve_path(&new_dir.to_string_lossy());

    if directories_differ && new_dir.exists() {
        return Err(format!(
            "Skill directory already exists at {}",
            new_dir.display()
        ));
    }

    if directories_differ {
        std::fs::rename(&old_dir, &new_dir).map_err(|error| error.to_string())?;
    }

    let new_path = new_dir.join("SKILL.md");
    let write_result = (|| -> Result<(), String> {
        let raw = std::fs::read_to_string(&new_path).map_err(|error| error.to_string())?;
        let (mut frontmatter, body) = parse_md_content(&raw);
        frontmatter.insert("name".into(), Value::from(new_name));
        write_md_file(&new_path, &frontmatter, &body)
            .map_err(|_| "Failed to write agent markdown file".to_string())
    })();
    if let Err(error) = write_result {
        if directories_differ
            && new_dir.exists()
            && !old_dir.exists()
            && let Err(rollback_error) = std::fs::rename(&new_dir, &old_dir)
        {
            tracing::error!(
                "Failed to rollback skill rename from {} to {}: {rollback_error}",
                new_dir.display(),
                old_dir.display()
            );
        }
        return Err(error);
    }

    tracing::info!(
        "Renamed skill: {old_name} -> {new_name} (path: {})",
        new_path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ompchamber-skills-{tag}-{}-{}",
            std::process::id(),
            rand_postfix()
        ));
        std::fs::create_dir_all(&dir).expect("temp root");
        dir
    }

    fn rand_postfix() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        COUNTER.fetch_add(1, Ordering::SeqCst)
    }

    fn write_skill_md(dir: &Path, name: &str, description: &str, body: &str) -> PathBuf {
        std::fs::create_dir_all(dir).expect("skill dir");
        let path = dir.join("SKILL.md");
        std::fs::write(
            &path,
            format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n"),
        )
        .expect("write");
        path
    }

    #[test]
    fn skill_name_validation() {
        assert!(is_valid_skill_name("a"));
        assert!(is_valid_skill_name("abc-123"));
        assert!(!is_valid_skill_name(""));
        assert!(!is_valid_skill_name("-abc"));
        assert!(!is_valid_skill_name("abc-"));
        assert!(!is_valid_skill_name("Abc"));
        assert!(!is_valid_skill_name(&"x".repeat(65)));
        assert!(is_valid_skill_name(&"x".repeat(64)));
        let error = assert_valid_skill_name("Invalid_Name").expect_err("must throw");
        assert!(error.contains("Invalid skill name"));
    }

    #[test]
    fn discovers_repository_local_agents_skills() {
        let root = temp_root("discover");
        let project = root.join("project");
        std::fs::create_dir_all(project.join(".git")).expect("git");
        let skill_dir = project
            .join(".agents")
            .join("skills")
            .join("repo-local-skill");
        write_skill_md(
            &skill_dir,
            "repo-local-skill",
            "Repository-local agents skill",
            "Use this skill in this repository.",
        );

        let discovered = discover_skills(Some(&project), &root);
        let matched = discovered
            .iter()
            .find(|skill| skill.name == "repo-local-skill")
            .expect("found");
        assert_eq!(matched.scope.as_deref(), Some("project"));
        assert_eq!(matched.source.as_deref(), Some("agents"));
        assert_eq!(
            matched.path.as_deref(),
            Some(skill_dir.join("SKILL.md").to_string_lossy().as_ref())
        );
        assert_eq!(
            matched.description.as_deref(),
            Some("Repository-local agents skill")
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn merge_dedupes_primary_first() {
        let skill = |name: &str| DiscoveredSkill {
            name: name.into(),
            path: Some(format!("/home/x/{name}")),
            ..Default::default()
        };
        let merged = merge_discovered_skills(
            &[
                skill("existing-opencode-skill"),
                skill("existing-agent-skill"),
            ],
            &[skill("existing-agent-skill"), skill("new-agent-skill")],
        );
        let names: Vec<&str> = merged.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "existing-opencode-skill",
                "existing-agent-skill",
                "new-agent-skill"
            ]
        );
    }

    #[test]
    fn built_in_sources_without_file_metadata() {
        let root = temp_root("builtin");
        let sources = get_skill_sources(
            "customize-opencode",
            Some(Path::new("/tmp/ompchamber-skills-test-missing-project")),
            Some(&DiscoveredSkill {
                name: "customize-opencode".into(),
                path: Some(BUILT_IN_SKILL_LOCATION.into()),
                scope: Some("user".into()),
                source: Some("opencode".into()),
                description: Some("Customize opencode".into()),
                content: Some(
                    "# Customizing opencode\n\nUse this skill when updating config.".into(),
                ),
            }),
            &root,
        )
        .expect("sources");

        let md = &sources["md"];
        assert_eq!(md["exists"], json!(true));
        assert_eq!(md["path"], Value::Null);
        assert_eq!(md["dir"], Value::Null);
        assert_eq!(md["scope"], json!("user"));
        assert_eq!(md["source"], json!("opencode"));
        assert_eq!(md["description"], json!("Customize opencode"));
        assert_eq!(
            md["instructions"],
            json!("# Customizing opencode\n\nUse this skill when updating config.")
        );
        assert_eq!(md["fields"], json!(["description", "instructions"]));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn unreadable_discovered_path_clears_metadata() {
        let root = temp_root("unreadable");
        let missing_path = root.join("missing-file").join("SKILL.md");
        let sources = get_skill_sources(
            "missing-agent-skill",
            Some(Path::new("/tmp/ompchamber-skills-test-missing-project")),
            Some(&DiscoveredSkill {
                name: "missing-agent-skill".into(),
                path: Some(missing_path.to_string_lossy().into_owned()),
                scope: Some("user".into()),
                source: Some("agents".into()),
                description: Some("Missing skill".into()),
                content: None,
            }),
            &root,
        )
        .expect("sources");

        let md = &sources["md"];
        assert_eq!(md["exists"], json!(false));
        assert_eq!(md["path"], Value::Null);
        assert_eq!(md["scope"], Value::Null);
        assert_eq!(md["description"], json!("Missing skill"));
        assert_eq!(md["instructions"], json!(""));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn enriches_real_markdown_locations() {
        let root = temp_root("enrich");
        let skill_dir = root.join("example-skill");
        write_skill_md(
            &skill_dir,
            "example-skill",
            "Example from agents",
            "Use this skill for examples.",
        );

        let sources = get_skill_sources(
            "example-skill",
            None,
            Some(&DiscoveredSkill {
                name: "example-skill".into(),
                path: Some(skill_dir.join("SKILL.md").to_string_lossy().into_owned()),
                scope: Some("user".into()),
                source: Some("agents".into()),
                description: Some("Fallback description".into()),
                content: None,
            }),
            &root,
        )
        .expect("sources");

        let md = &sources["md"];
        assert_eq!(md["exists"], json!(true));
        assert_eq!(
            md["path"],
            json!(skill_dir.join("SKILL.md").to_string_lossy().as_ref())
        );
        assert_eq!(md["scope"], json!("user"));
        assert_eq!(md["source"], json!("agents"));
        assert_eq!(md["description"], json!("Example from agents"));
        assert_eq!(md["instructions"], json!("Use this skill for examples."));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn creates_updates_deletes_skills() {
        let root = temp_root("crud");
        let project = root.join("project");
        std::fs::create_dir_all(&project).expect("mkdir");

        create_skill(
            "my-skill",
            &json!({ "description": "Does things", "instructions": "Body" }),
            Some(&project),
            Some("project"),
            &root,
        )
        .expect("create");
        let path = project
            .join(".opencode")
            .join("skills")
            .join("my-skill")
            .join("SKILL.md");
        assert!(path.exists());
        let raw = std::fs::read_to_string(&path).expect("read");
        assert!(raw.contains("name: my-skill"));
        assert!(raw.contains("description: Does things"));
        assert!(raw.contains("Body"));

        let error = create_skill(
            "my-skill",
            &json!({ "description": "dup" }),
            Some(&project),
            Some("project"),
            &root,
        )
        .expect_err("must throw");
        assert!(error.contains("already exists"));

        let error = create_skill(
            "no-description",
            &json!({ "instructions": "x" }),
            Some(&project),
            Some("project"),
            &root,
        )
        .expect_err("must throw");
        assert_eq!(error, "Skill description is required");

        create_skill(
            "agents-skill",
            &json!({ "description": "Agents target", "source": "agents" }),
            Some(&project),
            Some("project"),
            &root,
        )
        .expect("create agents");
        assert!(
            project
                .join(".agents")
                .join("skills")
                .join("agents-skill")
                .join("SKILL.md")
                .exists()
        );

        update_skill(
            "my-skill",
            &json!({ "instructions": "New body", "license": "MIT" }),
            Some(&project),
            None,
            &root,
        )
        .expect("update");
        let raw = std::fs::read_to_string(&path).expect("read");
        assert!(raw.contains("New body"));
        assert!(raw.contains("license: MIT"));

        let error = update_skill("missing-skill", &json!({}), Some(&project), None, &root)
            .expect_err("must throw");
        assert!(error.contains("not found"));

        delete_skill("my-skill", Some(&project), &root).expect("delete");
        assert!(!path.exists());
        let error = delete_skill("my-skill", Some(&project), &root).expect_err("must throw");
        assert!(error.contains("not found"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn supporting_files_round_trip_and_containment() {
        let root = temp_root("support");
        let skill_dir = root.join("sk");
        std::fs::create_dir_all(&skill_dir).expect("mkdir");
        write_skill_md(&skill_dir, "sk", "d", "b");

        write_skill_supporting_file(&skill_dir, "notes/deep.md", "inner").expect("write");
        assert_eq!(
            read_skill_supporting_file(&skill_dir, "notes/deep.md").expect("read"),
            Some("inner".to_string())
        );
        let listed = list_skill_supporting_files(&skill_dir);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["path"], json!("notes/deep.md"));

        assert!(read_skill_supporting_file(&skill_dir, "../escape").is_err());
        assert!(write_skill_supporting_file(&skill_dir, "../escape", "x").is_err());

        delete_skill_supporting_file(&skill_dir, "notes/deep.md").expect("delete");
        assert!(!skill_dir.join("notes").exists());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn renames_skill_directory_preserving_body_and_support() {
        let root = temp_root("rename");
        let project = root.join("project");
        let skill_dir = project
            .join(".opencode")
            .join("skills")
            .join("original-skill");
        std::fs::create_dir_all(&skill_dir).expect("mkdir");
        let body = "# Original Skill\n\nPreserve this non-trivial body across rename.\n\n## Details\n\n- step one\n- step two";
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: original-skill\ndescription: Original skill description\nlicense: MIT\n---\n\n{body}\n"),
        )
        .expect("write");
        std::fs::write(skill_dir.join("notes.md"), "supporting file contents\n").expect("write");

        rename_skill("original-skill", "renamed-skill", Some(&project), &root).expect("rename");

        let renamed_dir = project
            .join(".opencode")
            .join("skills")
            .join("renamed-skill");
        assert!(!skill_dir.exists());
        assert!(renamed_dir.join("SKILL.md").exists());
        assert_eq!(
            std::fs::read_to_string(renamed_dir.join("notes.md")).expect("read"),
            "supporting file contents\n"
        );
        let raw = std::fs::read_to_string(renamed_dir.join("SKILL.md")).expect("read");
        assert!(raw.contains("license: MIT"));
        assert!(raw.contains("name: renamed-skill"));
        assert!(raw.contains(body));

        let sources = get_skill_sources(
            "renamed-skill",
            Some(&project),
            Some(&DiscoveredSkill {
                name: "renamed-skill".into(),
                path: Some(renamed_dir.join("SKILL.md").to_string_lossy().into_owned()),
                scope: Some("project".into()),
                source: Some("opencode".into()),
                description: Some("fallback".into()),
                content: None,
            }),
            &root,
        )
        .expect("sources");
        assert_eq!(sources["md"]["name"], json!("renamed-skill"));
        assert_eq!(
            sources["md"]["description"],
            json!("Original skill description")
        );
        assert_eq!(sources["md"]["instructions"], json!(body));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn rename_rejections() {
        let root = temp_root("rename-reject");
        let project = root.join("project");
        let managed = project
            .join(".opencode")
            .join("skills")
            .join("managed-skill");
        write_skill_md(&managed, "managed-skill", "Managed", "Managed body");
        let conflict = project.join(".opencode").join("skills").join("taken-name");
        write_skill_md(&conflict, "taken-name", "Taken", "Taken body");
        let mismatch = project.join(".opencode").join("skills").join("folder-name");
        write_skill_md(&mismatch, "frontmatter-name", "Mismatch", "Mismatch body");
        let cache = root
            .join(".cache")
            .join("opencode")
            .join("skills")
            .join("stamp")
            .join("cache-skill");
        write_skill_md(&cache, "cache-skill", "Cache skill", "Cache body");

        assert!(
            rename_skill("managed-skill", "Invalid_Name", Some(&project), &root)
                .expect_err("must throw")
                .contains("Invalid skill name")
        );
        assert!(
            rename_skill("missing-skill", "new-skill", Some(&project), &root)
                .expect_err("must throw")
                .contains("not found")
        );
        assert!(
            rename_skill("managed-skill", "taken-name", Some(&project), &root)
                .expect_err("must throw")
                .contains("already exists")
        );
        assert!(
            rename_skill("folder-name", "renamed-mismatch", Some(&project), &root)
                .expect_err("must throw")
                .contains("does not match")
        );
        assert!(
            rename_skill("cache-skill", "cache-renamed", Some(&project), &root)
                .expect_err("must throw")
                .contains("managed skill directories")
        );

        assert!(managed.exists());
        assert!(cache.exists());
        assert!(
            !project
                .join(".opencode")
                .join("skills")
                .join("renamed-mismatch")
                .exists()
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn managed_path_and_inside_helpers() {
        let root = temp_root("managed");
        let project = root.join("project");
        let skill_md = project
            .join(".opencode")
            .join("skills")
            .join("s")
            .join("SKILL.md");
        assert!(is_managed_skill_path(
            skill_md.to_string_lossy().as_ref(),
            Some(&project),
            &root
        ));
        assert!(!is_managed_skill_path(
            BUILT_IN_SKILL_LOCATION,
            Some(&project),
            &root
        ));
        let outside = root.join("outside").join("SKILL.md");
        assert!(!is_managed_skill_path(
            outside.to_string_lossy().as_ref(),
            Some(&project),
            &root
        ));

        assert!(is_path_inside("/a/b", "/a"));
        assert!(is_path_inside("/a", "/a"));
        assert!(!is_path_inside("/ab", "/a"));
        assert!(!is_path_inside("", "/a"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn cache_root_skills_discover_and_are_not_managed() {
        let root = temp_root("cache");
        let project = root.join("project");
        std::fs::create_dir_all(project.join(".git")).expect("git");
        let cache_skill = root
            .join(".cache")
            .join("opencode")
            .join("skills")
            .join("stamp")
            .join("cache-skill");
        write_skill_md(&cache_skill, "cache-skill", "Cache skill", "Cache body");

        let discovered = discover_skills(Some(&project), &root);
        assert!(discovered.iter().any(|skill| skill.name == "cache-skill"));
        assert!(!is_managed_skill_path(
            cache_skill.join("SKILL.md").to_string_lossy().as_ref(),
            Some(&project),
            &root
        ));
        std::fs::remove_dir_all(&root).ok();
    }
}
