//! Port of `server/lib/skills-catalog/scan.js` (`scanSkillsRepository`):
//! clone a skills repository shallowly, list `SKILL.md` files (sparse
//! checkout fast path with an `ls-tree` fallback), parse YAML frontmatter,
//! and return catalog items. Root-level `SKILL.md` files are ignored (skill
//! name == directory name convention).
//!
//! GAP NOTES vs the JS implementation:
//! * YAML frontmatter parsing is a hand-rolled subset (no `yaml` crate on
//!   the allowed dependency list): flat `key: value` mappings with quoted
//!   scalars, plain scalars (null/bool/number/string inference), folded
//!   continuation lines, and `|`/`>` block scalars with `-`/`+` chomping.
//!   Anchors, aliases, tags, flow collections (`{}`/`[]`, stored as null),
//!   and multi-document streams are not supported; unsupported constructs
//!   degrade to the failed-parse warning exactly like a `yaml` throw.
//! * `localeCompare` ordering is approximated with case-insensitive then
//!   byte-wise comparison (ASCII skill names order identically).
//!
//! 中文说明：本模块实现 skill 仓库扫描主流程：浅克隆 → 枚举 SKILL.md
//! → 解析 frontmatter → 产出目录项。与 JS 实现的差异（YAML 子集、排序
//! 近似）见上方 GAP NOTES；根级 SKILL.md 依"skill 名 == 目录名"约定被
//! 忽略。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::skills_catalog::error::CatalogError;
use crate::skills_catalog::git::{
    GitIdentity, GitResult, GitRunOptions, GitRunner, assert_git_available, looks_like_auth_error,
    resolve_runner,
};
use crate::skills_catalog::source::{SourceParseResult, parse_skill_repo_source};

/// One scanned skill (`scan.js` item shape; `undefined` fields omitted).
/// 中文说明：字段与 scan.js 输出项一一对应，可选项用 skip 省略以复刻
/// JS 的 undefined 序列化行为。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SkillCatalogItem {
    /// 所属仓库的来源字符串（用户输入的 source 原样保留）。
    pub repo_source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// 生效的仓库子目录（未指定时序列化省略）。
    pub repo_subpath: Option<String>,
    /// 仓库内 skill 目录的 POSIX 相对路径。
    pub skill_dir: String,
    /// skill 名（约定等于目录名）。
    pub skill_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// frontmatter 中的 name 字段（可选）。
    pub frontmatter_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// frontmatter 中的 description 字段（可选）。
    pub description: Option<String>,
    /// 目录名是否为合法的 OpenCode skill 名（决定能否安装）。
    pub installable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// 采集到的警告列表（无警告时省略）。
    pub warnings: Option<Vec<String>>,
}

/// `scanSkillsRepository` result: `{ ok, normalizedRepo, effectiveSubpath,
/// items }` on success, `{ ok: false, error }` on failure.
/// 中文说明：成功与失败两种形状互斥，靠 Option 的省略序列化表达。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ScanResult {
    /// 扫描是否成功。
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// 规范化后的仓库标识（仅成功时存在）。
    pub normalized_repo: Option<String>,
    #[serde(default)]
    /// 生效的子目录；恒序列化（None → null），与 JS 形状保持一致。
    pub effective_subpath: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// 扫描出的 skill 列表（仅成功时存在）。
    pub items: Option<Vec<SkillCatalogItem>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// 失败原因（仅失败时存在）。
    pub error: Option<CatalogError>,
}

/// ScanResult 的构造辅助，维护 ok 标志与其余字段的互斥关系。
impl ScanResult {
    /// 构造成功结果：填入 repo/subpath/items，error 置空。
    pub fn ok(
        normalized_repo: String,
        effective_subpath: Option<String>,
        items: Vec<SkillCatalogItem>,
    ) -> Self {
        ScanResult {
            ok: true,
            normalized_repo: Some(normalized_repo),
            effective_subpath,
            items: Some(items),
            error: None,
        }
    }

    /// 构造失败结果：仅携带 error，其余字段清空。
    pub fn err(error: CatalogError) -> Self {
        ScanResult {
            ok: false,
            normalized_repo: None,
            effective_subpath: None,
            items: None,
            error: Some(error),
        }
    }
}

/// `scanSkillsRepository({ source, subpath, defaultSubpath, identity })`.
/// 中文说明：source 解析失败、git 不可用、临时目录创建失败等前置错误
/// 都以 ScanResult::err 形式返回而非 panic。
#[derive(Default, Clone)]
pub struct ScanParams {
    /// 仓库来源（owner/repo 或 URL）；None 视为空串并报 invalidSource。
    pub source: Option<String>,
    /// 显式指定的仓库子目录（可选）。
    pub subpath: Option<String>,
    /// source 未携带子目录时使用的兜底子目录（可选）。
    pub default_subpath: Option<String>,
    /// 克隆时使用的 git identity（带 SSH key 时改走 SSH URL）。
    pub identity: Option<GitIdentity>,
    /// Test seam; `None` runs the real `git` binary.
    /// 中文说明：生产路径传 None；测试注入 FakeGit 以离线驱动扫描流程。
    pub git_runner: Option<std::sync::Arc<dyn GitRunner>>,
}

/// SKILL.md 缺少 frontmatter 定界符时的警告文案。
const MISSING_FRONTMATTER_WARNING: &str = "Invalid SKILL.md: missing YAML frontmatter delimiter";
/// frontmatter YAML 解析失败时的警告文案。
const BAD_YAML_WARNING: &str = "Invalid SKILL.md: failed to parse YAML frontmatter";
/// 目录名不符合 skill 命名规则时的警告文案。
const INVALID_NAME_WARNING: &str = "Skill directory name is not a valid OpenCode skill name";

/// `/^[a-z0-9][a-z0-9-]*[a-z0-9]$|^[a-z0-9]$/` plus the 1–64 length bound.
/// 首尾必须是字母数字，中间仅允许小写字母/数字/连字符。
pub(crate) fn validate_skill_name(skill_name: &str) -> bool {
    let bytes = skill_name.as_bytes();
    if bytes.is_empty() || bytes.len() > 64 {
        return false;
    }
    let is_alnum = |byte: u8| byte.is_ascii_digit() || byte.is_ascii_lowercase();
    if !is_alnum(bytes[0]) {
        return false;
    }
    if bytes.len() == 1 {
        return true;
    }
    if !bytes.iter().all(|&byte| is_alnum(byte) || byte == b'-') {
        return false;
    }
    is_alnum(bytes[bytes.len() - 1])
}

/// SKILL.md 的解析产物：frontmatter 映射与累积的警告。
pub(crate) struct ParsedSkillMd {
    /// 解析出的 frontmatter 键值对（解析失败时为空 map）。
    frontmatter: Map<String, Value>,
    /// 逐条累积的警告（缺定界符 / YAML 失败 / 命名非法等）。
    warnings: Vec<String>,
}

/// `mkdtemp` without the tempfile crate: create a uniquely named directory
/// under the system temp dir.
/// 随机后缀重试至多 16 次：撞名则继续，其它 IO 错误立即返回。
pub(crate) fn mkd_temp(prefix: &str) -> std::io::Result<PathBuf> {
    use rand::Rng;
    let mut rng = rand::rng();
    for _ in 0..16 {
        let suffix: String = (0..10)
            .map(|_| {
                let alphabet = b"abcdefghijklmnopqrstuvwxyz0123456789";
                alphabet[rng.random_range(0..alphabet.len())] as char
            })
            .collect();
        let dir = std::env::temp_dir().join(format!("{prefix}{suffix}"));
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::other("mkdtemp: too many collisions"))
}

/// `safeRm`: recursive removal, errors ignored.
/// 用于扫描结束后的临时目录清理；失败（如已被删除）不影响扫描结果。
pub(crate) fn safe_rm(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

/// `cloneRepo` (scan flavor: 60s budget): preferred partial clone with
/// fallback; `Err` carries the fallback failure.
/// 优先带 `--filter=blob:none` 的 partial clone；仅当两次克隆都失败才返回 Err（携带回退失败的输出）。
async fn clone_repo(
    git: &dyn GitRunner,
    clone_url: &str,
    temp_dir: &Path,
    identity: &Option<GitIdentity>,
    timeout_ms: u64,
) -> Result<(), GitResult> {
    let run = |args: Vec<String>| {
        git.run(
            &args,
            &GitRunOptions {
                timeout_ms: Some(timeout_ms),
                identity: identity.clone(),
                ..GitRunOptions::default()
            },
        )
    };

    let preferred = vec![
        "clone".to_string(),
        "--depth".to_string(),
        "1".to_string(),
        "--filter=blob:none".to_string(),
        "--no-checkout".to_string(),
        clone_url.to_string(),
        temp_dir.to_string_lossy().to_string(),
    ];
    if run(preferred).await.ok {
        return Ok(());
    }

    let fallback = vec![
        "clone".to_string(),
        "--depth".to_string(),
        "1".to_string(),
        "--no-checkout".to_string(),
        clone_url.to_string(),
        temp_dir.to_string_lossy().to_string(),
    ];
    let fallback_result = run(fallback).await;
    if fallback_result.ok {
        return Ok(());
    }
    Err(fallback_result)
}

/// Map a failed clone to the auth/network error shapes.
/// 依据 stderr+message 判定认证失败（authRequired）或网络失败（networkError）。
pub(crate) fn clone_error(result: &GitResult) -> CatalogError {
    let message = format!("{}\n{}", result.stderr, result.message)
        .trim()
        .to_string();
    if looks_like_auth_error(&message) {
        return CatalogError::auth_required_ssh();
    }
    if message.is_empty() {
        return CatalogError::network("Failed to clone repository");
    }
    CatalogError::network(message)
}

/// scan.js `toFsPath`: join non-empty `/`-separated parts (no trimming).
/// 空段被丢弃，等价 JS 的 split('/').filter(Boolean) 后逐段 join。
fn to_fs_path(repo_dir: &Path, repo_rel_posix_path: &str) -> PathBuf {
    let mut path = repo_dir.to_path_buf();
    for part in repo_rel_posix_path
        .split('/')
        .filter(|part| !part.is_empty())
    {
        path.push(part);
    }
    path
}

/// `path.posix.dirname`.
/// 无分隔符返回 "."，根路径返回 "/"。
fn posix_dirname(path: &str) -> String {
    match path.rfind('/') {
        Some(0) => "/".to_string(),
        Some(index) => path[..index].to_string(),
        None => ".".to_string(),
    }
}

/// `path.posix.basename`.
/// 无分隔符时返回整个字符串。
fn posix_basename(path: &str) -> String {
    match path.rfind('/') {
        Some(index) => path[index + 1..].to_string(),
        None => path.to_string(),
    }
}

/// Parse `ls-files`/`ls-tree` stdout into SKILL.md paths.
/// 逐行 trim（兼容 \r），仅保留以 SKILL.md 结尾的路径。
fn parse_skill_md_listing(stdout: &str) -> Vec<String> {
    stdout
        .split('\n')
        .map(|line| line.trim_end_matches('\r').trim())
        .filter(|line| !line.is_empty())
        .filter(|line| line.ends_with("/SKILL.md") || *line == "SKILL.md")
        .map(str::to_string)
        .collect()
}

/// `scanSkillsRepository` port. See module docs for gaps.
/// 外层编排：校验 git → 解析 source → 选择 clone URL → 建临时目录 →
/// 委托 scan_inner；无论成败最后都删除临时目录。
pub async fn scan_skills_repository(params: ScanParams) -> ScanResult {
    let git = resolve_runner(params.git_runner.clone());
    let identity = params.identity.clone();

    if let Err(error) = assert_git_available(git.as_ref()).await {
        return ScanResult::err(error);
    }

    let parsed = match parse_skill_repo_source(
        params.source.as_deref().unwrap_or(""),
        params.subpath.as_deref(),
    ) {
        SourceParseResult::Ok(parsed) => parsed,
        SourceParseResult::Err(error) => return ScanResult::err(error),
    };

    let effective_subpath = parsed.effective_subpath.clone().or_else(|| {
        params
            .default_subpath
            .as_deref()
            .map(str::trim)
            .filter(|subpath| !subpath.is_empty())
            .map(str::to_string)
    });

    let use_ssh = params
        .identity
        .as_ref()
        .and_then(|identity| identity.ssh_key.as_deref())
        .is_some_and(|key| !key.is_empty());
    let clone_url = if use_ssh {
        parsed.clone_url_ssh.clone()
    } else {
        parsed.clone_url_https.clone()
    };

    let temp_base = match mkd_temp("ompchamber-skills-scan-") {
        Ok(dir) => dir,
        Err(error) => {
            return ScanResult::err(CatalogError::unknown(format!(
                "Failed to create temporary directory: {error}"
            )));
        }
    };

    let result = scan_inner(
        git.as_ref(),
        &params,
        &parsed,
        effective_subpath,
        &clone_url,
        &identity,
        &temp_base,
    )
    .await;

    safe_rm(&temp_base);
    result
}

/// The body of the JS `try` block (so temp cleanup runs on every path).
/// 中文说明：快路径为 sparse-checkout 后从磁盘读取；任何一步失败都回退
/// 到 ls-tree 枚举 + git show 逐 blob 读取，两条路径产出相同 items。
#[allow(clippy::too_many_arguments)]
async fn scan_inner(
    git: &dyn GitRunner,
    params: &ScanParams,
    parsed: &crate::skills_catalog::source::ParsedSource,
    effective_subpath: Option<String>,
    clone_url: &str,
    identity: &Option<GitIdentity>,
    temp_base: &Path,
) -> ScanResult {
    let cloned = clone_repo(git, clone_url, temp_base, identity, 60_000).await;
    if let Err(failure) = cloned {
        return ScanResult::err(clone_error(&failure));
    }

    let temp_base_str = temp_base.to_string_lossy().to_string();
    let run_git = |args: Vec<String>, timeout_ms: u64| {
        git.run(
            &args,
            &GitRunOptions {
                timeout_ms: Some(timeout_ms),
                identity: identity.clone(),
                ..GitRunOptions::default()
            },
        )
    };

    // Fast path: sparse checkout only SKILL.md files, then read from disk.
    let patterns: Vec<String> = match &effective_subpath {
        Some(subpath) => vec![
            format!("{subpath}/SKILL.md"),
            format!("{subpath}/**/SKILL.md"),
        ],
        None => vec!["SKILL.md".to_string(), "**/SKILL.md".to_string()],
    };

    let mut skill_md_paths: Option<Vec<String>> = None;

    let sparse_init = run_git(
        vec![
            "-C".to_string(),
            temp_base_str.clone(),
            "sparse-checkout".to_string(),
            "init".to_string(),
            "--no-cone".to_string(),
        ],
        15_000,
    )
    .await;
    if sparse_init.ok {
        let mut set_args = vec![
            "-C".to_string(),
            temp_base_str.clone(),
            "sparse-checkout".to_string(),
            "set".to_string(),
        ];
        set_args.extend(patterns.iter().cloned());
        let sparse_set = run_git(set_args, 30_000).await;
        if sparse_set.ok {
            let checkout = run_git(
                vec![
                    "-C".to_string(),
                    temp_base_str.clone(),
                    "checkout".to_string(),
                    "--force".to_string(),
                    "HEAD".to_string(),
                ],
                60_000,
            )
            .await;
            if checkout.ok {
                let ls_files = run_git(
                    vec![
                        "-C".to_string(),
                        temp_base_str.clone(),
                        "ls-files".to_string(),
                    ],
                    15_000,
                )
                .await;
                if ls_files.ok {
                    skill_md_paths = Some(parse_skill_md_listing(&ls_files.stdout));
                }
            }
        }
    }

    // Fallback: list the tree and read blobs via git.
    let skill_md_paths = match skill_md_paths {
        Some(paths) => paths,
        None => {
            let mut list_args = vec![
                "-C".to_string(),
                temp_base_str.clone(),
                "ls-tree".to_string(),
                "-r".to_string(),
                "--name-only".to_string(),
                "HEAD".to_string(),
            ];
            if let Some(subpath) = &effective_subpath {
                list_args.push("--".to_string());
                list_args.push(subpath.clone());
            }
            let list_result = run_git(list_args, 30_000).await;
            if !list_result.ok {
                // If the subpath doesn't exist, treat as an empty scan.
                return ScanResult::ok(
                    parsed.normalized_repo.clone(),
                    effective_subpath,
                    Vec::new(),
                );
            }
            parse_skill_md_listing(&list_result.stdout)
        }
    };

    // Root-level SKILL.md doesn't map to the folder-name convention.
    let mut seen = HashSet::new();
    let mut unique_skill_dirs: Vec<String> = Vec::new();
    for path in &skill_md_paths {
        if path == "SKILL.md" {
            continue;
        }
        let dir = posix_dirname(path);
        if seen.insert(dir.clone()) {
            unique_skill_dirs.push(dir);
        }
    }

    let mut items: Vec<SkillCatalogItem> = Vec::new();
    for skill_dir in unique_skill_dirs {
        let skill_name = posix_basename(&skill_dir);
        let skill_md_path = format!("{skill_dir}/SKILL.md");

        let mut warnings: Vec<String> = Vec::new();
        let mut content = String::new();

        let file_path = to_fs_path(temp_base, &skill_md_path);
        match std::fs::read_to_string(&file_path) {
            Ok(text) => content = text,
            Err(_) => {
                let show = run_git(
                    vec![
                        "-C".to_string(),
                        temp_base_str.clone(),
                        "show".to_string(),
                        format!("HEAD:{skill_md_path}"),
                    ],
                    15_000,
                )
                .await;
                if !show.ok {
                    warnings.push("Failed to read SKILL.md".to_string());
                } else {
                    content = show.stdout;
                }
            }
        }

        let parsed_md = parse_skill_md(&content);
        warnings.extend(parsed_md.warnings);

        let description = parsed_md
            .frontmatter
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string);
        let frontmatter_name = parsed_md
            .frontmatter
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string);

        let installable = validate_skill_name(&skill_name);
        if !installable {
            warnings.push(INVALID_NAME_WARNING.to_string());
        }

        items.push(SkillCatalogItem {
            repo_source: params.source.clone().unwrap_or_default(),
            repo_subpath: effective_subpath.clone(),
            skill_dir,
            skill_name,
            frontmatter_name,
            description,
            installable,
            warnings: if warnings.is_empty() {
                None
            } else {
                Some(warnings)
            },
        });
    }

    // Stable ordering for UX (localeCompare approximation).
    items.sort_by(|a, b| {
        a.skill_name
            .to_lowercase()
            .cmp(&b.skill_name.to_lowercase())
            .then_with(|| a.skill_name.cmp(&b.skill_name))
    });

    ScanResult::ok(parsed.normalized_repo.clone(), effective_subpath, items)
}

/// JS frontmatter regex: `^---\r?\n([\s\S]*?)\r?\n---\r?\n([\s\S]*)$`.
/// Returns `(frontmatter, rest)`.
/// 中文说明：手写字节扫描替代 regex；空 frontmatter（定界符间无分隔换行）与 JS 一致地判定失败。
fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let after_open = text
        .strip_prefix("---\r\n")
        .or_else(|| text.strip_prefix("---\n"))?;

    let bytes = after_open.as_bytes();
    for index in 0..bytes.len() {
        if bytes[index] != b'\n' {
            continue;
        }
        let tail = &after_open[index + 1..];
        let (closing_len, hit) = if tail.starts_with("---\n") {
            (5, true)
        } else if tail.starts_with("---\r\n") {
            (6, true)
        } else {
            (0, false)
        };
        if hit {
            let frontmatter = after_open[..index]
                .strip_suffix('\r')
                .unwrap_or(&after_open[..index]);
            return Some((frontmatter, &after_open[index + closing_len..]));
        }
    }
    None
}

/// 解析 SKILL.md：先拆 frontmatter 再走 YAML 子集解析；
/// 缺定界符与解析失败分别映射为固定警告文案，且 frontmatter 置空。
pub(crate) fn parse_skill_md(content: &str) -> ParsedSkillMd {
    let Some((frontmatter, _rest)) = split_frontmatter(content) else {
        return ParsedSkillMd {
            frontmatter: Map::new(),
            warnings: vec![MISSING_FRONTMATTER_WARNING.to_string()],
        };
    };

    match parse_frontmatter_yaml(frontmatter) {
        Ok(map) => ParsedSkillMd {
            frontmatter: map,
            warnings: Vec::new(),
        },
        Err(_yaml_error) => ParsedSkillMd {
            frontmatter: Map::new(),
            warnings: vec![BAD_YAML_WARNING.to_string()],
        },
    }
}

/// Minimal YAML frontmatter subset parser (see module GAP NOTES).
/// 中文说明：仅支持平铺 mapping；标量/序列文档不报错但按空 map 处理
/// （JS 里 frontmatter?.description 同样读不到任何字段）。
fn parse_frontmatter_yaml(input: &str) -> Result<Map<String, Value>, String> {
    let lines: Vec<&str> = input.lines().collect();

    // Base indent comes from the first content line; structural lines sit at
    // that column and nested content below it.
    let base_indent = lines
        .iter()
        .find(|line| {
            let line = *line;
            !line.trim().is_empty() && !line.trim_start().starts_with('#')
        })
        .map(|line| indent_of(line))
        .unwrap_or(0);

    let mut map: Map<String, Value> = Map::new();
    let mut saw_mapping = false;
    let mut saw_scalar_doc = false;
    let mut index = 0;

    while index < lines.len() {
        let line = lines[index];
        index += 1;

        // Tabs may appear inside values but never as indentation.
        let leading_whitespace = line.len() - line.trim_start_matches([' ', '\t']).len();
        if line[..leading_whitespace].contains('\t') {
            return Err("tab indentation".to_string());
        }

        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let indent = indent_of(line);
        if indent > base_indent {
            // Nested content under a previous key (or a block scalar already
            // consumed); skip.
            continue;
        }
        if indent < base_indent {
            return Err("bad indentation".to_string());
        }

        // Sequence entries / bare scalars make the document a non-mapping.
        if trimmed == "-" || trimmed.starts_with("- ") || is_scalar_doc_line(trimmed) {
            if saw_mapping {
                return Err("mixed mapping and scalar document".to_string());
            }
            saw_scalar_doc = true;
            continue;
        }

        let Some((key, value_part)) = split_key_value(trimmed) else {
            if saw_mapping {
                return Err(format!("invalid mapping line: {trimmed}"));
            }
            saw_scalar_doc = true;
            continue;
        };
        if saw_scalar_doc {
            return Err("mixed mapping and scalar document".to_string());
        }

        let value = parse_mapping_value(value_part, &lines, &mut index, base_indent)?;
        map.insert(key, value);
        saw_mapping = true;
    }

    if saw_scalar_doc {
        // A scalar/sequence document parses fine in JS but is not a mapping:
        // `frontmatter?.description` reads nothing. Represent it as an empty
        // map (only string fields are ever consumed).
        return Ok(Map::new());
    }
    Ok(map)
}

/// Hack-free helper: count leading spaces.
/// 仅统计空格，tab 不计入缩进。
fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

/// Lines that begin with a non-key character are scalar-document lines
/// (directives, flow fragments, punctuation) rather than mappings.
/// 首字符不是字母数字/下划线/引号即判定为标量文档行。
fn is_scalar_doc_line(trimmed: &str) -> bool {
    let first = trimmed.as_bytes()[0];
    !first.is_ascii_alphanumeric() && first != b'_' && first != b'"' && first != b'\''
}

/// Split `key: value` (colon followed by a space or end of line).
/// 冒号必须位于行尾或后跟空格才算键值分隔；空键返回 None。
fn split_key_value(line: &str) -> Option<(String, &str)> {
    let bytes = line.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b':' && (index + 1 == bytes.len() || bytes[index + 1] == b' ') {
            let key = line[..index].trim();
            let value = if index + 1 == bytes.len() {
                ""
            } else {
                line[index + 1..].trim_start()
            };
            if key.is_empty() {
                return None;
            }
            return Some((unquote_key(key)?, value));
        }
        index += 1;
    }
    None
}

/// 键去引号：双引号键走转义解析，单引号键把 '' 还原为 '，裸含引号视为非法。
fn unquote_key(key: &str) -> Option<String> {
    if let Some(inner) = key
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    {
        return unescape_double_quoted(inner).ok();
    }
    if let Some(inner) = key
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
    {
        return Some(inner.replace("''", "'"));
    }
    if key.contains('"') || key.contains('\'') {
        return None;
    }
    Some(key.to_string())
}

/// Parse the value part of a mapping line, consuming block scalars and
/// folded continuation lines from `lines` starting at `index`.
/// 中文说明：普通标量会把更深缩进的续行折叠为空格拼接。
fn parse_mapping_value(
    value_part: &str,
    lines: &[&str],
    index: &mut usize,
    key_indent: usize,
) -> Result<Value, String> {
    // Block scalars: `|`, `|-`, `|+`, `>`, `>-`, `>+` (comment may follow).
    if let Some((indicator, chomp)) = split_block_indicator(value_part) {
        let content = collect_block_scalar(lines, index, key_indent);
        return Ok(Value::String(render_block_scalar(
            &content, indicator, chomp,
        )));
    }

    // Empty value: nested content (skipped) or null.
    if value_part.is_empty() || value_part.starts_with('#') {
        return Ok(Value::Null);
    }

    // Flow collections parse as non-strings in JS; store null and let only
    // string fields matter. (Malformed flow YAML diverges: no warning.)
    if value_part.starts_with('{') || value_part.starts_with('[') {
        return Ok(Value::Null);
    }

    if let Some(rest) = value_part.strip_prefix('\'') {
        let (value, trailer) = scan_single_quoted(rest)?;
        expect_blank_or_comment(trailer)?;
        return Ok(Value::String(value));
    }

    if let Some(rest) = value_part.strip_prefix('"') {
        let (value, trailer) = scan_double_quoted(rest)?;
        expect_blank_or_comment(trailer)?;
        return Ok(Value::String(value));
    }

    // Plain scalar: strip a trailing comment, fold continuation lines.
    let mut value = strip_trailing_comment(value_part).trim_end().to_string();
    while *index < lines.len() {
        let candidate = lines[*index];
        let candidate_trimmed = candidate.trim();
        if candidate_trimmed.is_empty() {
            break;
        }
        if indent_of(candidate) <= key_indent {
            break;
        }
        value.push(' ');
        value.push_str(candidate_trimmed);
        *index += 1;
    }

    Ok(plain_scalar_value(&value))
}

/// 识别 `|`/`>` 块标量指示符及其 chomp 标记（`-`/`+`/空白）；
/// 指示符后仅允许注释，否则不视为块标量。
fn split_block_indicator(value_part: &str) -> Option<(char, char)> {
    let mut chars = value_part.chars();
    let first = chars.next()?;
    if first != '|' && first != '>' {
        return None;
    }
    let second = chars.next().unwrap_or(' ');
    let chomp = match second {
        '-' => '-',
        '+' => '+',
        ' ' | '\t' => ' ',
        _ => return None,
    };
    let trailer: String = chars.collect();
    let trailer = trailer.trim();
    if trailer.is_empty() || trailer.starts_with('#') {
        Some((first, chomp))
    } else {
        None
    }
}

/// Collect block scalar lines (blank or indented deeper than the key).
/// 以首个内容行的缩进为内容基准；缩进回落到键缩进即结束，行首基准缩进被剥离。
fn collect_block_scalar(lines: &[&str], index: &mut usize, key_indent: usize) -> Vec<String> {
    let mut collected: Vec<String> = Vec::new();
    let mut content_indent: Option<usize> = None;
    while *index < lines.len() {
        let line = lines[*index];
        let trimmed = line.trim();
        if trimmed.is_empty() {
            collected.push(String::new());
            *index += 1;
            continue;
        }
        let indent = indent_of(line);
        if indent <= key_indent {
            break;
        }
        if let Some(content_indent) = content_indent {
            if indent < content_indent {
                break;
            }
        } else {
            content_indent = Some(indent);
        }
        let strip = content_indent.unwrap_or(indent);
        collected.push(line.chars().skip(strip).collect());
        *index += 1;
    }
    collected
}

/// `|` 按字面保留换行；`>` 把单个换行折叠为空格、空行还原为换行；
/// chomp 标记决定末尾换行的剪裁（- 去除 / 默认保留一个 / + 尽量保留）。
fn render_block_scalar(lines: &[String], indicator: char, chomp: char) -> String {
    // Drop trailing blank lines for clip chomping decisions.
    let mut lines = lines.to_vec();
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }

    let mut body = if indicator == '|' {
        lines.join("\n")
    } else {
        // Folded: single newlines become spaces, blank lines become newlines.
        let mut folded = String::new();
        let mut pending_break = false;
        for line in &lines {
            if line.is_empty() {
                folded.push('\n');
                pending_break = false;
                continue;
            }
            if pending_break {
                folded.push(' ');
            }
            folded.push_str(line);
            pending_break = true;
        }
        folded
    };

    match chomp {
        '-' => {}
        '+' => {
            // Keep: original trailing blanks were preserved by the caller's
            // copy; approximate by restoring a single trailing break per
            // stripped blank (rarely used).
            body.push('\n');
        }
        _ => {
            if !body.is_empty() {
                body.push('\n');
            }
        }
    }
    body
}

/// 仅当 `#` 前有空白才视为注释起点（值中间的 # 不受影响，与 YAML 规则一致）。
fn strip_trailing_comment(value: &str) -> &str {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index + 1 < bytes.len() {
        if bytes[index] == b' ' && bytes[index + 1] == b'#' {
            return &value[..index];
        }
        index += 1;
    }
    value
}

/// 引号标量闭合后只允许空白或注释，出现其它内容即报错。
fn expect_blank_or_comment(trailer: &str) -> Result<(), String> {
    let trailer = trailer.trim();
    if trailer.is_empty() || trailer.starts_with('#') {
        Ok(())
    } else {
        Err("unexpected content after quoted scalar".to_string())
    }
}

/// 扫描至闭合单引号；`''` 转义为单个 `'`。返回 (值, 闭合后的剩余尾部)。
fn scan_single_quoted(rest: &str) -> Result<(String, &str), String> {
    let mut value = String::new();
    let mut chars = rest.char_indices();
    while let Some((index, ch)) = chars.next() {
        if ch == '\'' {
            if rest[index + 1..].starts_with('\'') {
                value.push('\'');
                chars.next();
                continue;
            }
            return Ok((value, &rest[index + 1..]));
        }
        value.push(ch);
    }
    Err("unterminated single-quoted scalar".to_string())
}

/// 扫描至闭合双引号；支持常用转义与 \uXXXX，非法转义或未闭合均报错。
fn scan_double_quoted(rest: &str) -> Result<(String, &str), String> {
    let mut value = String::new();
    let mut chars = rest.char_indices();
    while let Some((index, ch)) = chars.next() {
        if ch == '"' {
            return Ok((value, &rest[index + 1..]));
        }
        if ch == '\\' {
            let Some((_, escape)) = chars.next() else {
                return Err("unterminated escape".to_string());
            };
            match escape {
                'n' => value.push('\n'),
                't' => value.push('\t'),
                'r' => value.push('\r'),
                '0' => value.push('\0'),
                '\\' => value.push('\\'),
                '"' => value.push('"'),
                '/' => value.push('/'),
                'u' => {
                    let hex: String = rest[index + 2..].chars().take(4).collect();
                    let code = u32::from_str_radix(&hex, 16)
                        .map_err(|_| "bad unicode escape".to_string())?;
                    value.push(char::from_u32(code).ok_or("bad unicode escape")?);
                    for _ in 0..4 {
                        chars.next();
                    }
                }
                other => return Err(format!("unsupported escape \\{other}")),
            }
            continue;
        }
        value.push(ch);
    }
    Err("unterminated double-quoted scalar".to_string())
}

/// 复用 scan_double_quoted 完成键内转义解析：补回闭合引号后整体扫描。
fn unescape_double_quoted(inner: &str) -> Result<String, String> {
    // The scanner needs the closing quote, which unquote_key already
    // stripped; re-append it.
    let requote = format!("{inner}\"");
    let (value, trailer) = scan_double_quoted(&requote)?;
    if !trailer.is_empty() {
        return Err("unexpected content in quoted key".to_string());
    }
    Ok(value)
}

/// Plain scalar typing: null / bool / number / string.
/// 中文说明：数字判定要求串内至少含一个 ASCII 数字，避免 "1e5" 之类歧义串被误判。
fn plain_scalar_value(value: &str) -> Value {
    match value {
        "null" | "Null" | "NULL" | "~" => Value::Null,
        "true" | "True" | "TRUE" => Value::Bool(true),
        "false" | "False" | "FALSE" => Value::Bool(false),
        _ => {
            if let Ok(int) = value.parse::<i64>() {
                return Value::from(int);
            }
            if value.parse::<f64>().is_ok()
                && value.chars().any(|c| c.is_ascii_digit())
                && let Ok(float) = value.parse::<f64>()
            {
                return serde_json::Number::from_f64(float)
                    .map(Value::Number)
                    .unwrap_or(Value::Null);
            }
            Value::String(value.to_string())
        }
    }
}

/// scan.rs 的单元与 fake-git 集成测试：命名校验、frontmatter 解析、
/// 错误映射与端到端扫描行为。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills_catalog::test_support::{
        FakeGit, scan_git_fake, ssh_identity, unique_temp_dir, write_skill_md,
    };
    /// scan_skills_repository 的直通包装，统一测试调用点。
    async fn ok_scan(params: ScanParams) -> ScanResult {
        scan_skills_repository(params).await
    }

    /// 契约：validate_skill_name 与 JS 正则一致——合法名通过；空串、大写、
    /// 下划线、首尾连字符、非 ASCII、超 64 位拒绝；恰好 64 位通过。
    #[test]
    fn validates_skill_names_like_the_js_pattern() {
        for name in ["a", "ab", "a-b", "skill-name-2", "0", "9x"] {
            assert!(validate_skill_name(name), "{name} should be valid");
        }
        for name in [
            "",
            "-a",
            "a-",
            "A",
            "aB",
            "a_b",
            "a b",
            ".a",
            "a.b",
            "-",
            "UPPER",
            "über",
            &"x".repeat(65),
        ] {
            assert!(!validate_skill_name(name), "{name} should be invalid");
        }
        assert!(validate_skill_name(&"x".repeat(64)));
    }

    /// 契约：split_frontmatter 与 JS regex 等价——LF/CRLF 均正确拆分；
    /// 缺闭合定界符或空 frontmatter（无分隔换行）返回 None。
    #[test]
    fn splits_frontmatter_like_the_js_regex() {
        let (front, rest) = split_frontmatter("---\nname: x\n---\nbody").expect("split");
        assert_eq!(front, "name: x");
        assert_eq!(rest, "body");

        let (front, rest) =
            split_frontmatter("---\r\nname: x\r\n---\r\nbody\r\n").expect("split crlf");
        assert_eq!(front, "name: x");
        assert_eq!(rest, "body\r\n");

        assert!(split_frontmatter("---\nno closing delimiter").is_none());
        assert!(
            split_frontmatter("---\n---\nrest").is_none(),
            "empty frontmatter has no separator newline"
        );
        assert!(split_frontmatter("").is_none());
        assert_eq!(
            split_frontmatter("---\n\n---\nrest").map(|(f, r)| (f.to_string(), r)),
            Some(("".to_string(), "rest"))
        );
    }

    /// 契约：frontmatter 的引号值、类型字面量、注释剥离、块标量（|/|-/>）
    /// 与普通标量续行折叠均解析出正确类型与文本。
    #[test]
    fn parses_frontmatter_fields() {
        let parsed = parse_skill_md("---\nname: my-skill\ndescription: Does things\n---\nbody");
        assert!(parsed.warnings.is_empty());
        assert_eq!(
            parsed.frontmatter.get("name").and_then(Value::as_str),
            Some("my-skill")
        );
        assert_eq!(
            parsed
                .frontmatter
                .get("description")
                .and_then(Value::as_str),
            Some("Does things")
        );

        // Quoted values.
        let parsed = parse_skill_md("---\ndescription: \"quoted \\\"value\\\"\"\n---\nx");
        assert_eq!(
            parsed
                .frontmatter
                .get("description")
                .and_then(Value::as_str),
            Some("quoted \"value\"")
        );

        let parsed = parse_skill_md("---\ndescription: 'it''s here'\n---\nx");
        assert_eq!(
            parsed
                .frontmatter
                .get("description")
                .and_then(Value::as_str),
            Some("it's here")
        );

        // Non-string scalars are typed (JS ignores them for name/description).
        let parsed = parse_skill_md("---\nstars: 42\nflag: true\nnothing: null\n---\nx");
        assert_eq!(parsed.frontmatter.get("stars"), Some(&Value::from(42)));
        assert_eq!(parsed.frontmatter.get("flag"), Some(&Value::Bool(true)));
        assert_eq!(parsed.frontmatter.get("nothing"), Some(&Value::Null));

        // Comments and trailing comment stripping.
        let parsed =
            parse_skill_md("---\n# leading comment\ndescription: value # trailing\n---\nx");
        assert_eq!(
            parsed
                .frontmatter
                .get("description")
                .and_then(Value::as_str),
            Some("value")
        );

        // Block scalars.
        let parsed = parse_skill_md("---\ndescription: |\n  line one\n  line two\n---\nx");
        assert_eq!(
            parsed
                .frontmatter
                .get("description")
                .and_then(Value::as_str),
            Some("line one\nline two\n")
        );
        let parsed = parse_skill_md("---\ndescription: |-\n  stripped\n---\nx");
        assert_eq!(
            parsed
                .frontmatter
                .get("description")
                .and_then(Value::as_str),
            Some("stripped")
        );
        let parsed = parse_skill_md("---\ndescription: >\n  folded\n  lines\n---\nx");
        assert_eq!(
            parsed
                .frontmatter
                .get("description")
                .and_then(Value::as_str),
            Some("folded lines\n")
        );

        // Plain scalar continuation folding.
        let parsed = parse_skill_md("---\ndescription: first\n  second\n---\nx");
        assert_eq!(
            parsed
                .frontmatter
                .get("description")
                .and_then(Value::as_str),
            Some("first second")
        );
    }

    /// 契约：缺定界符与 YAML 解析失败（含 tab 缩进）分别产生对应固定警告，
    /// 且 frontmatter 为空 map。
    #[test]
    fn frontmatter_failures_produce_warnings() {
        let parsed = parse_skill_md("no frontmatter at all");
        assert_eq!(parsed.warnings, vec![MISSING_FRONTMATTER_WARNING]);
        assert!(parsed.frontmatter.is_empty());

        let parsed = parse_skill_md("---\nname: x\nunparseable ][ line\n---\nx");
        assert_eq!(parsed.warnings, vec![BAD_YAML_WARNING]);
        assert!(parsed.frontmatter.is_empty());

        // Tab indentation fails like yaml.parse.
        let parsed = parse_skill_md("---\nname: x\n\tbad: tab\n---\nx");
        assert_eq!(parsed.warnings, vec![BAD_YAML_WARNING]);
    }

    /// 契约：source 缺失在发起克隆之前即被拒绝，错误 kind 为 invalidSource。
    #[tokio::test]
    async fn rejects_invalid_sources_before_cloning() {
        let result = ok_scan(ScanParams {
            source: None,
            ..Default::default()
        })
        .await;
        assert!(!result.ok);
        let error = result.error.expect("error");
        assert_eq!(error.kind, "invalidSource");
        assert_eq!(error.message, "Repository source is required");
    }

    /// 契约：git 二进制不可用时返回 gitUnavailable 错误。
    #[tokio::test]
    async fn reports_git_unavailable() {
        let git = FakeGit::new(|_| GitResult {
            message: "spawn git ENOENT".to_string(),
            ..GitResult::default()
        });
        let result = ok_scan(ScanParams {
            source: Some("owner/repo".to_string()),
            git_runner: Some(git),
            ..Default::default()
        })
        .await;
        assert!(!result.ok);
        let error = result.error.expect("error");
        assert_eq!(error.kind, "gitUnavailable");
    }

    /// 契约：克隆失败的 publickey 类 stderr 映射为 authRequired 且 sshOnly=true。
    #[tokio::test]
    async fn maps_clone_auth_failures_to_auth_required() {
        let git = FakeGit::new(|args| {
            if args.first().map(String::as_str) == Some("--version") {
                return GitResult::success("git version 2.0.0\n", "");
            }
            GitResult {
                stderr: "git@github.com: Permission denied (publickey).\nfatal: Could not read from remote repository.".to_string(),
                message: "Command failed: git clone".to_string(),
                ..GitResult::default()
            }
        });
        let result = ok_scan(ScanParams {
            source: Some("owner/private-repo".to_string()),
            git_runner: Some(git),
            ..Default::default()
        })
        .await;
        let error = result.error.expect("error");
        assert_eq!(error.kind, "authRequired");
        assert_eq!(error.ssh_only, Some(true));
        assert_eq!(
            error.message,
            "Authentication required to access this repository"
        );
    }

    /// 契约：非认证类克隆失败映射为 networkError，message 携带 stderr 原文。
    #[tokio::test]
    async fn maps_clone_network_failures_with_stderr_text() {
        let git = FakeGit::new(|args| {
            if args.first().map(String::as_str) == Some("--version") {
                return GitResult::success("git version 2.0.0\n", "");
            }
            GitResult {
                stderr: "fatal: unable to access 'https://x/': timed out".to_string(),
                message: "Command failed: git clone".to_string(),
                ..GitResult::default()
            }
        });
        let result = ok_scan(ScanParams {
            source: Some("owner/repo".to_string()),
            git_runner: Some(git),
            ..Default::default()
        })
        .await;
        let error = result.error.expect("error");
        assert_eq!(error.kind, "networkError");
        assert_eq!(
            error.message,
            "fatal: unable to access 'https://x/': timed out\nCommand failed: git clone"
        );
    }

    /// 契约：失败输出为空时 networkError 使用固定 fallback 文案。
    #[tokio::test]
    async fn empty_network_error_uses_the_fallback_message() {
        let git = FakeGit::new(|args| {
            if args.first().map(String::as_str) == Some("--version") {
                return GitResult::success("git version 2.0.0\n", "");
            }
            GitResult::default()
        });
        let result = ok_scan(ScanParams {
            source: Some("owner/repo".to_string()),
            git_runner: Some(git),
            ..Default::default()
        })
        .await;
        let error = result.error.expect("error");
        assert_eq!(error.kind, "networkError");
        assert_eq!(error.message, "Failed to clone repository");
    }

    /// 构造扫描 fixture：合法、命名非法、无 frontmatter 三个 skill，
    /// 外加根级 SKILL.md 与一个非 SKILL.md 文件。
    fn scan_fixture() -> std::path::PathBuf {
        let dir = unique_temp_dir("skills-scan-fixture");
        write_skill_md(
            &dir.join("skills/good-skill"),
            "name: good-skill\ndescription: A good skill\n",
            "instructions\n",
        );
        write_skill_md(&dir.join("skills/Bad_Name"), "", "no frontmatter\n");
        write_skill_md(&dir.join("skills/plain"), "name: plain-skill\n", "body\n");
        std::fs::write(dir.join("skills/plain/extra.txt"), "extra").expect("write");
        write_skill_md(&dir, "name: root-skill\n", "root\n");
        dir
    }

    /// 遍历 fixture 生成 fake git 的文件清单（仅 SKILL.md 相对路径，每行一个）。
    fn listing_for(dir: &Path) -> String {
        let mut listing = String::new();
        // 递归收集目录下的 SKILL.md 相对路径，模拟 ls-files/ls-tree 输出。
        fn walk(dir: &Path, prefix: &str, listing: &mut String) {
            for entry in std::fs::read_dir(dir).expect("read dir") {
                let entry = entry.expect("entry");
                let name = entry.file_name().to_string_lossy().to_string();
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, &format!("{prefix}{name}/"), listing);
                } else if name == "SKILL.md" {
                    listing.push_str(&format!("{prefix}{name}\n"));
                }
            }
        }
        walk(dir, "", &mut listing);
        listing
    }

    /// 契约：端到端扫描排除根级 SKILL.md、按名排序返回三条 item；
    /// installable/warnings 字段正确；sparse 模式包含生效的 subpath。
    #[tokio::test]
    async fn scans_skills_from_a_cloned_repository() {
        let fixture = scan_fixture();
        let listing = listing_for(&fixture);
        let git = scan_git_fake(Some(fixture.clone()), listing);

        let result = ok_scan(ScanParams {
            source: Some("anthropics/skills".to_string()),
            default_subpath: Some("skills".to_string()),
            git_runner: Some(git.clone()),
            ..Default::default()
        })
        .await;

        assert!(result.ok, "scan failed: {:?}", result.error);
        assert_eq!(result.normalized_repo.as_deref(), Some("anthropics/skills"));
        assert_eq!(result.effective_subpath.as_deref(), Some("skills"));

        let items = result.items.expect("items");
        assert_eq!(items.len(), 3, "root SKILL.md excluded: {items:?}");
        // Sorted by skill name (locale approximation).
        let names: Vec<&str> = items.iter().map(|item| item.skill_name.as_str()).collect();
        assert_eq!(names, vec!["Bad_Name", "good-skill", "plain"]);

        let good = items
            .iter()
            .find(|item| item.skill_name == "good-skill")
            .expect("good");
        assert!(good.installable);
        assert_eq!(good.frontmatter_name.as_deref(), Some("good-skill"));
        assert_eq!(good.description.as_deref(), Some("A good skill"));
        assert_eq!(good.skill_dir, "skills/good-skill");
        assert_eq!(good.repo_source, "anthropics/skills");
        assert_eq!(good.repo_subpath.as_deref(), Some("skills"));
        assert!(good.warnings.is_none());

        let bad = items
            .iter()
            .find(|item| item.skill_name == "Bad_Name")
            .expect("bad");
        assert!(!bad.installable);
        assert_eq!(
            bad.warnings,
            Some(vec![
                MISSING_FRONTMATTER_WARNING.to_string(),
                INVALID_NAME_WARNING.to_string(),
            ])
        );

        // Sparse patterns include the effective subpath.
        let sparse_sets: Vec<Vec<String>> = git
            .calls()
            .into_iter()
            .filter(|call| call.len() > 4 && call[2] == "sparse-checkout" && call[3] == "set")
            .collect();
        assert_eq!(sparse_sets.len(), 1);
        assert_eq!(
            sparse_sets[0][4..],
            vec![
                "skills/SKILL.md".to_string(),
                "skills/**/SKILL.md".to_string()
            ]
        );
        std::fs::remove_dir_all(&fixture).ok();
    }

    /// 契约：携带 SSH key 的 identity 使唯一一次 clone 走 git@ SSH URL。
    #[tokio::test]
    async fn ssh_identity_selects_the_ssh_clone_url() {
        let fixture = scan_fixture();
        let listing = listing_for(&fixture);
        let git = scan_git_fake(Some(fixture.clone()), listing);

        let result = ok_scan(ScanParams {
            source: Some("owner/repo".to_string()),
            identity: Some(ssh_identity()),
            git_runner: Some(git.clone()),
            ..Default::default()
        })
        .await;

        assert!(result.ok, "scan failed: {:?}", result.error);
        let clones = git.find_calls_starting_with(&["clone"]);
        assert_eq!(clones.len(), 1);
        assert_eq!(
            clones[0][5], "git@github.com:owner/repo.git",
            "ssh identity must use the SSH URL: {:?}",
            clones[0]
        );
        std::fs::remove_dir_all(&fixture).ok();
    }

    /// 契约：sparse-checkout 不可用时回退 ls-tree 枚举 + git show 逐 blob
    /// 读取，产出与快路径一致的扫描结果。
    #[tokio::test]
    async fn falls_back_to_ls_tree_and_git_show_when_sparse_fails() {
        let fixture = scan_fixture();
        let listing = listing_for(&fixture);
        let git = FakeGit::new(move |args| {
            if args.first().map(String::as_str) == Some("--version") {
                return GitResult::success("git version 2.0.0\n", "");
            }
            if args.first().map(String::as_str) == Some("clone") {
                // Clone without checking anything out: only git show can
                // read the blobs.
                return GitResult::success("", "");
            }
            if args.len() >= 3 && args[0] == "-C" {
                match args[2].as_str() {
                    "sparse-checkout" => {
                        return GitResult {
                            stderr: "this repo has no sparse support".to_string(),
                            message: "Command failed".to_string(),
                            ..GitResult::default()
                        };
                    }
                    "ls-tree" => return GitResult::success(listing.clone(), ""),
                    "show" => {
                        let path = args.last().map(String::as_str).unwrap_or("");
                        let content =
                            std::fs::read_to_string(fixture.join(path.trim_start_matches("HEAD:")))
                                .unwrap_or_default();
                        return GitResult::success(content, "");
                    }
                    _ => return GitResult::success("", ""),
                }
            }
            GitResult::success("", "")
        });

        let result = ok_scan(ScanParams {
            source: Some("owner/repo".to_string()),
            git_runner: Some(git.clone()),
            ..Default::default()
        })
        .await;

        assert!(result.ok, "scan failed: {:?}", result.error);
        let items = result.items.expect("items");
        assert_eq!(items.len(), 3, "git show fallback reads blobs: {items:?}");
        let good = items
            .iter()
            .find(|item| item.skill_name == "good-skill")
            .expect("good");
        assert_eq!(good.description.as_deref(), Some("A good skill"));
    }

    /// 契约：subpath 不存在导致 ls-tree 失败时按空扫描成功返回，而非报错。
    #[tokio::test]
    async fn missing_subpath_ls_tree_failure_is_an_empty_scan() {
        let git = FakeGit::new(|args| {
            if args.first().map(String::as_str) == Some("--version") {
                return GitResult::success("git version 2.0.0\n", "");
            }
            if args.len() >= 3 && args[0] == "-C" && args[2] == "sparse-checkout" {
                return GitResult {
                    stderr: "no sparse".to_string(),
                    message: "Command failed".to_string(),
                    ..GitResult::default()
                };
            }
            if args.len() >= 3 && args[0] == "-C" && args[2] == "ls-tree" {
                return GitResult {
                    stderr: "fatal: path 'missing' does not exist".to_string(),
                    message: "Command failed".to_string(),
                    ..GitResult::default()
                };
            }
            GitResult::success("", "")
        });

        let result = ok_scan(ScanParams {
            source: Some("owner/repo".to_string()),
            subpath: Some("missing".to_string()),
            git_runner: Some(git),
            ..Default::default()
        })
        .await;

        assert!(result.ok, "empty scan: {:?}", result.error);
        assert_eq!(result.items, Some(Vec::new()));
        assert_eq!(result.effective_subpath.as_deref(), Some("missing"));
    }

    /// 契约：成功/失败两种 ScanResult 序列化出的 JSON 与 JS 形状完全一致
    /// （undefined 字段省略、effectiveSubpath 恒存在）。
    #[tokio::test]
    async fn scan_result_serializes_to_the_js_shape() {
        let result = ScanResult::ok(
            "a/b".to_string(),
            None,
            vec![SkillCatalogItem {
                repo_source: "a/b".to_string(),
                repo_subpath: None,
                skill_dir: "skills/x".to_string(),
                skill_name: "x".to_string(),
                frontmatter_name: None,
                description: None,
                installable: false,
                warnings: Some(vec![INVALID_NAME_WARNING.to_string()]),
            }],
        );
        let value = serde_json::to_value(&result).expect("serialize");
        assert_eq!(
            value,
            serde_json::json!({
                "ok": true,
                "normalizedRepo": "a/b",
                "effectiveSubpath": null,
                "items": [{
                    "repoSource": "a/b",
                    "skillDir": "skills/x",
                    "skillName": "x",
                    "installable": false,
                    "warnings": [INVALID_NAME_WARNING],
                }],
            })
        );

        let failed = ScanResult::err(CatalogError::auth_required_ssh());
        let value = serde_json::to_value(&failed).expect("serialize");
        assert_eq!(
            value,
            serde_json::json!({
                "ok": false,
                "effectiveSubpath": null,
                "error": {
                    "kind": "authRequired",
                    "message": "Authentication required to access this repository",
                    "sshOnly": true,
                },
            })
        );
    }
}
