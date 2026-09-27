//! Port of `server/lib/scheduled-tasks/loops.js` — markdown loop discovery.
//!
//! Loops are git-commit-able `.agents/loops/*.md` files (project scope,
//! ancestors up to the worktree root) plus `~/.agents/loops/*.md` (user
//! scope) parsed into scheduled-task definitions. Project scope shadows user
//! scope on name collision; malformed files surface as `definition: None`
//! entries so the scheduler keeps their task alive until the file is fixed.
//! 中文说明：markdown loop 发现层。loop 是可提交进 git 的
//! `.agents/loops/*.md`（项目作用域，沿祖先目录上溯到 worktree 根）与
//! `~/.agents/loops/*.md`（用户作用域），解析为调度任务定义；同名时
//! 项目作用域遮蔽用户作用域，坏文件以 `definition: None` 呈现，调度器
//! 在文件修复前保持任务存活。

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use super::md::{parse_md_document, user_loop_root, write_md_document};

/// loop 名称（任务名）的存储上限；超长直接拒绝而非截断，保证身份键与
/// 存量任务名一致。
pub const MAX_TASK_NAME_LENGTH: usize = 80;

/// loop 文件的作用域来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopScope {
/// `~/.agents/loops` 用户级文件。
    User,
/// 项目 `.agents/loops`（含祖先目录）文件。
    Project,
}

/// 序列化辅助。
impl LoopScope {
/// 小写字符串标识（"user"/"project"）。
    pub fn as_str(&self) -> &'static str {
        match self {
            LoopScope::User => "user",
            LoopScope::Project => "project",
        }
    }
}

/// 从 loop markdown 解析出的任务定义。
#[derive(Debug, Clone)]
pub struct LoopDefinition {
/// 任务名（frontmatter `name`，非空且不超过 80 字符）。
    pub name: String,
/// 是否启用（frontmatter `enabled` 为 true 才算）。
    pub enabled: bool,
/// cron 表达式（frontmatter `schedule`）。
    pub cron: String,
/// 可选时区（frontmatter `timezone`）。
    pub timezone: Option<String>,
/// markdown 正文，即执行 prompt。
    pub prompt: String,
/// `model` 字段按首个 `/` 拆出的 provider。
    pub provider_id: String,
/// `model` 字段的模型部分（可含 `/`）。
    pub model_id: String,
/// 可选 agent（frontmatter `agent`）。
    pub agent: Option<String>,
}

/// 转换为存储层输入。
impl LoopDefinition {
    /// The `{name, enabled, schedule, execution}` task-definition shape the
    /// project-config reconcile consumes (see loops.js field mapping).
/// 输出 `{name, enabled, schedule, execution}` 任务定义形状，供项目配置
/// 对账消费；timezone 与 agent 仅在存在时写入。
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

/// 发现的 loop：来源作用域 + 文件路径 + 解析结果（坏文件为 `None`）。
#[derive(Debug, Clone)]
pub struct DiscoveredLoop {
/// 发现作用域。
    pub scope: LoopScope,
/// markdown 文件路径。
    pub file_path: PathBuf,
/// 解析定义；文件不合法时为 `None`。
    pub definition: Option<LoopDefinition>,
}

/// 取字符串值并 trim，空白或非字符串返回 `None`。
fn as_non_empty_string(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Split `provider/model` on the first `/` (model ids may contain slashes).
/// 按首个 `/` 拆 `provider/model`；分隔符缺失或位于首尾则返回 `None`。
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
/// 解析单个 loop 文件：要求 frontmatter 的 `name`（不超过 80 字符）、
/// `schedule`、正文 prompt 与 `provider/model` 形式的 `model` 全部合法；
/// 任何缺失/非法都记录 warn 并返回 `None`，不影响其他 loop。
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
/// 在 frontmatter 写入 `enabled` 并保留其余字段与正文；文件当前不是
/// 合法 loop 时拒绝改写（返回 false），避免破坏手工内容。
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

/// 列出目录下（不递归）按文件名排序的 `.md` 文件；目录不可读返回空。
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
/// 从 start_dir（含自身）向上找最近的含 `.git` 祖先，即 worktree 根。
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
/// [startDir ..= stopDir] 的祖先链；stop_dir 为 None 或不可达时上溯到
/// 文件系统根。
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
/// 按作用域收集原始 loop 文件：用户目录在前，随后是项目祖先由近及远；
/// 项目根取 worktree 根（缺失则取项目绝对路径）。
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
/// 发现并解析全部 loop，实施遮蔽规则：先发现的同名项优先（项目祖先由
/// 近及远、用户文件在后），因此项目作用域天然遮蔽用户作用域；坏文件
/// 原样保留（definition 为 None）。
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

/// loop 发现层的单元测试：frontmatter 解析、遮蔽规则与文件改写。
#[cfg(test)]
mod tests {
    use super::*;

/// 带临时 `.git` 项目的自清理测试夹具。
    struct TempProject {
/// 临时根目录（drop 时整体清理）。
        root: PathBuf,
    }

/// 目录与 loop 写入辅助。
    impl TempProject {
/// 创建含 `repo/.git` 的唯一临时目录。
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

/// 项目目录路径。
        fn project(&self) -> PathBuf {
            self.root.join("repo")
        }

/// 把内容写入项目 `.agents/loops/<file>` 并返回路径。
        fn write_loop(&self, file: &str, content: &str) -> PathBuf {
            let dir = self.project().join(".agents").join("loops");
            std::fs::create_dir_all(&dir).expect("mkdir loops");
            let path = dir.join(file);
            std::fs::write(&path, content).expect("write loop");
            path
        }
    }

/// 测试结束清理整个临时目录。
    impl Drop for TempProject {
/// 递归删除临时根目录。
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

/// 合法 loop 文件的标准样例。
    const VALID: &str = "---\nname: daily-digest\nschedule: \"0 9 * * *\"\nenabled: true\nmodel: anthropic/claude-sonnet-4-5\nagent: plan\ntimezone: Europe/Kyiv\n---\nSummarize repository changes since yesterday.\n";

/// frontmatter 与正文正确映射为定义并转出任务输入。
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

/// model 按首个 `/` 拆分，缺省字段取默认。
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

/// 缺 name/schedule/model 或文件无 frontmatter 均解析失败。
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

/// 名称超过 80 字符被拒绝。
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

/// 从嵌套目录能发现祖先级的 loop 文件。
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

/// 坏文件保留为 definition None 且不影响合法文件。
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

/// 无 loop 文件时发现结果为空。
    #[test]
    fn returns_empty_list_when_nothing_exists() {
        let tmp = TempProject::new("empty");
        assert!(discover_loops(Some(&tmp.project())).is_empty());
    }

/// 切换 enabled 保留其余 frontmatter 与正文，坏文件不被改写。
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
