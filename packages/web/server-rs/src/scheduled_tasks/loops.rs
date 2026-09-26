//! Port of `server/lib/scheduled-tasks/loops.js` — markdown loop discovery.
//!
//! Loops are git-commit-able `.agents/loops/*.md` files (project scope,
//! ancestors up to the worktree root) plus `~/.agents/loops/*.md` (user
//! scope) parsed into scheduled-task definitions. Project scope shadows user
//! scope on name collision; malformed files surface as `definition: None`
//! entries so the scheduler keeps their task alive until the file is fixed.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use super::md::{parse_md_document, user_loop_root, write_md_document};

pub const MAX_TASK_NAME_LENGTH: usize = 80;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopScope {
    User,
    Project,
}

impl LoopScope {
    pub fn as_str(&self) -> &'static str {
        match self {
            LoopScope::User => "user",
            LoopScope::Project => "project",
        }
    }
}

#[derive(Debug, Clone)]
pub struct LoopDefinition {
    pub name: String,
    pub enabled: bool,
    pub cron: String,
    pub timezone: Option<String>,
    pub prompt: String,
    pub provider_id: String,
    pub model_id: String,
    pub agent: Option<String>,
}

impl LoopDefinition {
    /// The `{name, enabled, schedule, execution}` task-definition shape the
    /// project-config reconcile consumes (see loops.js field mapping).
    pub fn to_task_input(&self) -> Value {
        let mut schedule = Map::new();
        schedule.insert("kind".into(), json!("cron"));
        schedule.insert("cron".into(), json!(self.cron));
        if let Some(tz) = &self.timezone {
            schedule.insert("timezone".into(), json!(tz));
        }
        let mut execution = Map::new();
        execution.insert("prompt".into(), json!(self.prompt));
        execution.insert("providerID".into(), json!(self.provider_id));
        execution.insert("modelID".into(), json!(self.model_id));
        if let Some(agent) = &self.agent {
            execution.insert("agent".into(), json!(agent));
        }
        json!({
            "name": self.name,
            "enabled": self.enabled,
            "schedule": Value::Object(schedule),
            "execution": Value::Object(execution),
        })
    }
}

#[derive(Debug, Clone)]
pub struct DiscoveredLoop {
    pub scope: LoopScope,
    pub file_path: PathBuf,
    pub definition: Option<LoopDefinition>,
}

fn as_non_empty_string(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Split `provider/model` on the first `/` (model ids may contain slashes).
fn split_provider_model(value: &Value) -> Option<(String, String)> {
    let raw = as_non_empty_string(value)?;
    let separator = raw.find('/');
    match separator {
        Some(idx) if idx > 0 && idx < raw.len() - 1 => Some((
            raw[..idx].trim().to_string(),
            raw[idx + 1..].trim().to_string(),
        )),
        _ => None,
    }
}

/// Parse one loop markdown file, or `None` when malformed (warned, never
/// blocking other loops).
pub fn parse_loop_definition(file_path: &Path) -> Option<LoopDefinition> {
    let (frontmatter, body) = match parse_md_document(file_path) {
        Ok(parsed) => parsed,
        Err(error) => {
            tracing::warn!(
                "[loops] skipped malformed loop file {}: {error}",
                file_path.display()
            );
            return None;
        }
    };

    let name = as_non_empty_string(frontmatter.get("name").unwrap_or(&Value::Null));
    let Some(name) = name else {
        tracing::warn!(
            "[loops] skipped {}: frontmatter \"name\" is required",
            file_path.display()
        );
        return None;
    };
    if name.chars().count() > MAX_TASK_NAME_LENGTH {
        // Reject instead of clamping: identity keys must match stored names.
        tracing::warn!(
            "[loops] skipped {}: frontmatter \"name\" exceeds {MAX_TASK_NAME_LENGTH} characters",
            file_path.display()
        );
        return None;
    }

    let cron = as_non_empty_string(frontmatter.get("schedule").unwrap_or(&Value::Null));
    if cron.is_none() {
        tracing::warn!(
            "[loops] skipped {}: frontmatter \"schedule\" (cron expression) is required",
            file_path.display()
        );
        return None;
    }
    let cron = cron.unwrap_or_default();

    let prompt = as_non_empty_string(&Value::String(body));
    if prompt.is_none() {
        tracing::warn!(
            "[loops] skipped {}: markdown body (the execution prompt) is required",
            file_path.display()
        );
        return None;
    }
    let prompt = prompt.unwrap_or_default();

    let Some((provider_id, model_id)) =
        split_provider_model(frontmatter.get("model").unwrap_or(&Value::Null))
    else {
        tracing::warn!(
            "[loops] skipped {}: frontmatter \"model\" must be \"provider/model\"",
            file_path.display()
        );
        return None;
    };

    let timezone = as_non_empty_string(frontmatter.get("timezone").unwrap_or(&Value::Null));
    let agent = as_non_empty_string(frontmatter.get("agent").unwrap_or(&Value::Null));
    let enabled = frontmatter.get("enabled").and_then(Value::as_bool) == Some(true);

    Some(LoopDefinition {
        name,
        enabled,
        cron,
        timezone,
        prompt,
        provider_id,
        model_id,
        agent,
    })
}

/// Toggle `enabled` in the file's frontmatter, keeping every other field and
/// the body intact. `false` when the file is not currently a valid loop.
pub fn set_loop_file_enabled(file_path: &Path, enabled: bool) -> bool {
    if parse_loop_definition(file_path).is_none() {
        return false;
    }
    let Ok((mut frontmatter, body)) = parse_md_document(file_path) else {
        return false;
    };
    frontmatter.insert("enabled".into(), Value::Bool(enabled));
    write_md_document(file_path, &frontmatter, &body).is_ok()
}

fn walk_loop_md_files(root_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root_dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "md"))
        .collect();
    files.sort();
    files
}

/// `findWorktreeRoot`: nearest ancestor (inclusive) containing `.git`.
pub fn find_worktree_root(start_dir: &Path) -> Option<PathBuf> {
    let mut current = std::path::absolute(start_dir).ok()?;
    loop {
        if current.join(".git").exists() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

/// `getAncestors`: [startDir ..= stopDir] (or up to the filesystem root).
pub fn get_ancestors(start_dir: &Path, stop_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut result = Vec::new();
    let Ok(mut current) = std::path::absolute(start_dir) else {
        return result;
    };
    let stop = stop_dir.and_then(|d| std::path::absolute(d).ok());
    loop {
        result.push(current.clone());
        if stop.as_ref() == Some(&current) {
            break;
        }
        if !current.pop() {
            break;
        }
    }
    result
}

/// Raw loop files per scope: user files first, then project ancestors
/// nearest-first (`discoverLoopFiles`).
pub fn discover_loop_files(project_path: Option<&Path>) -> Vec<(PathBuf, LoopScope)> {
    let mut files = Vec::new();
    for path in walk_loop_md_files(&user_loop_root()) {
        files.push((path, LoopScope::User));
    }
    if let Some(project) = project_path {
        let worktree_root =
            find_worktree_root(project).or_else(|| std::path::absolute(project).ok());
        for ancestor in get_ancestors(project, worktree_root.as_deref()) {
            for path in walk_loop_md_files(&ancestor.join(".agents").join("loops")) {
                files.push((path, LoopScope::Project));
            }
        }
    }
    files
}

/// Discover and parse loops with scope shadowing: project scope shadows user
/// scope on name collision; among project files the nearest ancestor wins
/// (discovered first, so later user files cannot replace it).
pub fn discover_loops(project_path: Option<&Path>) -> Vec<DiscoveredLoop> {
    let mut loops: Vec<DiscoveredLoop> = Vec::new();
    let mut by_name: Vec<(String, DiscoveredLoop)> = Vec::new();

    for (file_path, scope) in discover_loop_files(project_path) {
        let Some(definition) = parse_loop_definition(&file_path) else {
            loops.push(DiscoveredLoop {
                scope,
                file_path,
                definition: None,
            });
            continue;
        };
        let name = definition.name.clone();
        if let Some((_, existing)) = by_name.iter_mut().find(|(n, _)| *n == name) {
            if existing.scope == LoopScope::Project || scope == LoopScope::User {
                continue;
            }
            *existing = DiscoveredLoop {
                scope,
                file_path: file_path.clone(),
                definition: Some(definition),
            };
            continue;
        }
        by_name.push((
            name,
            DiscoveredLoop {
                scope,
                file_path: file_path.clone(),
                definition: Some(definition),
            },
        ));
    }

    loops.extend(by_name.into_iter().map(|(_, entry)| entry));
    loops
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempProject {
        root: PathBuf,
    }

    impl TempProject {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "oc-loops-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.subsec_nanos())
                    .unwrap_or(0)
            ));
            let project = root.join("repo");
            std::fs::create_dir_all(project.join(".git")).expect("mkdir");
            Self { root }
        }

        fn project(&self) -> PathBuf {
            self.root.join("repo")
        }

        fn write_loop(&self, file: &str, content: &str) -> PathBuf {
            let dir = self.project().join(".agents").join("loops");
            std::fs::create_dir_all(&dir).expect("mkdir loops");
            let path = dir.join(file);
            std::fs::write(&path, content).expect("write loop");
            path
        }
    }

    impl Drop for TempProject {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    const VALID: &str = "---\nname: daily-digest\nschedule: \"0 9 * * *\"\nenabled: true\nmodel: anthropic/claude-sonnet-4-5\nagent: plan\ntimezone: Europe/Kyiv\n---\nSummarize repository changes since yesterday.\n";

    #[test]
    fn maps_frontmatter_and_body_to_definition() {
        let tmp = TempProject::new("parse");
        let path = tmp.write_loop("digest.md", VALID);
        let def = parse_loop_definition(&path).expect("valid");
        assert_eq!(def.name, "daily-digest");
        assert!(def.enabled);
        assert_eq!(def.cron, "0 9 * * *");
        assert_eq!(def.timezone.as_deref(), Some("Europe/Kyiv"));
        assert_eq!(def.provider_id, "anthropic");
        assert_eq!(def.model_id, "claude-sonnet-4-5");
        assert_eq!(def.agent.as_deref(), Some("plan"));
        assert_eq!(def.prompt, "Summarize repository changes since yesterday.");

        let input = def.to_task_input();
        assert_eq!(input["schedule"]["kind"], "cron");
        assert_eq!(input["schedule"]["cron"], "0 9 * * *");
        assert_eq!(input["execution"]["providerID"], "anthropic");
    }

    #[test]
    fn splits_model_ids_on_first_slash() {
        let tmp = TempProject::new("split");
        let path = tmp.write_loop(
            "nested.md",
            "---\nname: nested-model\nschedule: \"0 8 * * 1\"\nmodel: openai/gpt-5\n---\nRun weekly checks.\n",
        );
        let def = parse_loop_definition(&path).expect("valid");
        assert_eq!(def.provider_id, "openai");
        assert_eq!(def.model_id, "gpt-5");
        assert!(!def.enabled);
        assert!(def.agent.is_none());
    }

    #[test]
    fn rejects_missing_required_fields() {
        let tmp = TempProject::new("invalid");
        let no_name = tmp.project().join("noname.md");
        std::fs::write(
            &no_name,
            "---\nschedule: \"0 9 * * *\"\nmodel: openai/gpt-5\n---\nPrompt only.\n",
        )
        .expect("write");
        assert!(parse_loop_definition(&no_name).is_none());

        let no_schedule = tmp.project().join("noschedule.md");
        std::fs::write(
            &no_schedule,
            "---\nname: no-schedule\nmodel: openai/gpt-5\n---\nPrompt only.\n",
        )
        .expect("write");
        assert!(parse_loop_definition(&no_schedule).is_none());

        let no_model = tmp.project().join("nomodel.md");
        std::fs::write(
            &no_model,
            "---\nname: no-model\nschedule: \"0 9 * * *\"\n---\nPrompt only.\n",
        )
        .expect("write");
        assert!(parse_loop_definition(&no_model).is_none());

        let malformed = tmp.project().join("malformed.md");
        std::fs::write(&malformed, "not a markdown frontmatter file at all").expect("write");
        assert!(parse_loop_definition(&malformed).is_none());
    }

    #[test]
    fn rejects_names_longer_than_storage_limit() {
        let tmp = TempProject::new("long");
        let long_name = "x".repeat(81);
        let path = tmp.write_loop(
            "long.md",
            &format!(
                "---\nname: {long_name}\nschedule: \"0 9 * * *\"\nmodel: openai/gpt-5\n---\nRun.\n"
            ),
        );
        assert!(parse_loop_definition(&path).is_none());
    }

    #[test]
    fn discovers_project_loops_and_scans_ancestors() {
        let tmp = TempProject::new("ancestors");
        tmp.write_loop("root-loop.md", "---\nname: root-loop\nschedule: \"0 9 * * *\"\nmodel: openai/gpt-5\n---\nFrom the root.\n");

        let nested = tmp.project().join("src").join("nested");
        std::fs::create_dir_all(&nested).expect("mkdir nested");

        let loops = discover_loops(Some(&nested));
        assert_eq!(loops.len(), 1);
        assert_eq!(loops[0].scope, LoopScope::Project);
        assert_eq!(loops[0].definition.as_ref().expect("def").name, "root-loop");
        assert!(loops[0].file_path.ends_with(".agents/loops/root-loop.md"));
    }

    #[test]
    fn reports_malformed_files_without_blocking_valid_ones() {
        let tmp = TempProject::new("mixed");
        tmp.write_loop(
            "bad.md",
            "---\nname: bad\nschedule: \"0 9 * * *\"\n---\nNo model.\n",
        );
        tmp.write_loop(
            "good.md",
            "---\nname: good\nschedule: \"0 9 * * *\"\nmodel: openai/gpt-5\n---\nValid.\n",
        );

        let loops = discover_loops(Some(&tmp.project()));
        let bad = loops
            .iter()
            .find(|l| l.file_path.ends_with("bad.md"))
            .expect("bad kept");
        assert!(bad.definition.is_none());
        assert_eq!(bad.scope, LoopScope::Project);
        let good = loops
            .iter()
            .find(|l| l.file_path.ends_with("good.md"))
            .expect("good");
        assert_eq!(good.definition.as_ref().expect("def").name, "good");
    }

    #[test]
    fn returns_empty_list_when_nothing_exists() {
        let tmp = TempProject::new("empty");
        assert!(discover_loops(Some(&tmp.project())).is_empty());
    }

    #[test]
    fn set_loop_file_enabled_preserves_other_fields() {
        let tmp = TempProject::new("toggle");
        let path = tmp.write_loop(
            "daily.md",
            "---\nname: daily-digest\nschedule: \"0 9 * * *\"\nenabled: true\nmodel: openai/gpt-5\ncustom: keep-me\n---\n\nRun the digest.\n",
        );
        assert!(set_loop_file_enabled(&path, false));
        let content = std::fs::read_to_string(&path).expect("read");
        assert!(content.contains("enabled: false"));
        assert!(content.contains("custom: keep-me"));
        assert!(content.contains("Run the digest."));

        // Malformed loops are not rewritten.
        let malformed_path = tmp.project().join("malformed.md");
        std::fs::write(&malformed_path, "---\nname: daily-digest\n---\nRun.\n").expect("write");
        assert!(!set_loop_file_enabled(&malformed_path, false));
        assert_eq!(
            std::fs::read_to_string(&malformed_path).expect("read"),
            "---\nname: daily-digest\n---\nRun.\n"
        );
    }
}
