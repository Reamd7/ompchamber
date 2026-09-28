//! Port of `server/lib/skills-catalog/source.js` (`parseSkillRepoSource`):
//! parse a repository source string (HTTPS URL, SSH URL, or
//! `owner/repo[/subpath]` shorthand) into clone URLs plus the normalized
//! `owner/repo` identifier. The JS implementation is literal string surgery;
//! this port mirrors it branch for branch.
//!
//! 中文说明：把仓库源字符串（HTTPS URL、SSH URL 或 `owner/repo[/subpath]`
//! 简写）解析为克隆 URL 与归一化的 `owner/repo` 标识。JS 实现是逐分支的
//! 字符串手术，本移植逐分支对齐。

use crate::skills_catalog::error::CatalogError;

/// 简写格式默认指向的 git host。
const GITHUB_HOST: &str = "github.com";

/// Successful `parseSkillRepoSource` payload (`ok: true` fields).
/// 解析成功后的字段全集：host/owner/repo、两条 clone URL、生效 subpath
/// 与归一化 repo 标识。
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedSource {
    /// git host（简写固定 github.com）。
    pub host: String,
    /// 仓库所有者（URL 形式可为多级 `group/sub`）。
    pub owner: String,
    /// 仓库名（已剥离 `.git` 后缀）。
    pub repo: String,
    /// SSH 克隆 URL（`git@host:owner/repo.git`）。
    pub clone_url_ssh: String,
    /// HTTPS 克隆 URL（`https://host/owner/repo.git`）。
    pub clone_url_https: String,
    /// Subpath from `options.subpath` (SSH/HTTPS URLs) or from either the
    /// explicit option or the shorthand tail; `None` when absent.
    /// 显式 options.subpath 优先；简写尾巴次之；都没有则 None。
    pub effective_subpath: Option<String>,
    /// 归一化的 `owner/repo` 标识（缓存 key 的一部分）。
    pub normalized_repo: String,
}

/// Result of `parseSkillRepoSource`: parsed fields or an `invalidSource`
/// error carrying the exact JS message.
/// Ok 携带解析字段，Err 携带 invalidSource 错误（消息与 JS 一致）。
#[derive(Debug, Clone, PartialEq)]
pub enum SourceParseResult {
    /// 解析成功：host、owner、repo、clone URL 与 subpath。
    Ok(ParsedSource),
    /// 解析失败：invalidSource 错误。
    Err(CatalogError),
}

/// SourceParseResult 的判断辅助。
impl SourceParseResult {
    /// 是否解析成功。
    pub fn is_ok(&self) -> bool {
        matches!(self, SourceParseResult::Ok(_))
    }
}

/// JS `normalizeGitOwnerRepo`: trims, strips a trailing `.git` (case
/// insensitive) from the repo, and rejects empty owner/repo.
/// 任一侧为空则 None；repo 先剥 `.git`（大小写不敏感）。
fn normalize_git_owner_repo(owner: &str, repo: &str) -> Option<(String, String)> {
    let owner = owner.trim();
    let repo = repo.trim();
    let repo = strip_dot_git(repo);
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner.to_string(), repo.to_string()))
}

/// 剥离结尾的 `.git` 后缀（大小写不敏感），长度不足时原样返回。
fn strip_dot_git(value: &str) -> &str {
    if value.len() >= 4 && value[value.len() - 4..].eq_ignore_ascii_case(".git") {
        &value[..value.len() - 4]
    } else {
        value
    }
}

/// Port of `parseSkillRepoSource(input, { subpath })`.
///
/// * `input` — the source string (JS tolerates non-strings by treating them
///   as empty; callers here pass `""` for absent values).
/// * `subpath` — the explicit `options.subpath` override, when given.
/// 空输入报“源必填”；按前缀识别 https/ssh/简写三种格式：URL 形式从
/// host 与路径段提取 owner/repo（subpath 只认显式选项），简写形式按
/// 正则语义匹配 owner/repo[/尾巴]（显式 subpath 优先于尾巴）。
pub fn parse_skill_repo_source(input: &str, subpath: Option<&str>) -> SourceParseResult {
    let raw = input.trim();
    if raw.is_empty() {
        return SourceParseResult::Err(CatalogError::invalid_source(
            "Repository source is required",
        ));
    }
    let explicit_subpath: Option<String> = subpath
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let url_format = if raw.starts_with("https://") {
        "https"
    } else if raw.starts_with("git@") {
        "ssh"
    } else {
        "shorthand"
    };

    // JS: const gitHost = urlFormat === 'https' ? raw.split('/')[2]
    //                      : urlFormat === 'ssh' ? raw.split('@')[1].split(':')[0] : null;
    let git_host: Option<String> = match url_format {
        "https" => raw.split('/').nth(2).map(str::to_string),
        "ssh" => raw
            .split('@')
            .nth(1)
            .map(|rest| rest.split(':').next().unwrap_or("").to_string()),
        _ => None,
    };

    if git_host.is_none() && url_format != "shorthand" {
        return SourceParseResult::Err(CatalogError::invalid_source(
            "Invalid repository URL format",
        ));
    }

    // JS: const pathSegments = https ? raw.split('/').slice(3).filter(Boolean)
    //                          : ssh ? (raw.split('@')[1].split(':')[1] ?? '').split('/').filter(Boolean)
    //                          : null;
    let path_segments: Option<Vec<&str>> = match url_format {
        "https" => Some(
            raw.split('/')
                .skip(3)
                .filter(|segment| !segment.is_empty())
                .collect(),
        ),
        "ssh" => {
            let after_at = raw.split('@').nth(1).unwrap_or("");
            let after_colon = after_at.split(':').nth(1).unwrap_or("");
            Some(
                after_colon
                    .split('/')
                    .filter(|segment| !segment.is_empty())
                    .collect(),
            )
        }
        _ => None,
    };

    // JS: repoName = last path segment with `.git` stripped; an empty repo
    // (or empty owner) is rejected later by `normalizeGitOwnerRepo`.
    let repo_name: Option<&str> = path_segments
        .as_ref()
        .and_then(|segments| segments.last().map(|last| strip_dot_git(last)));

    let git_owner: Option<String> = path_segments.as_ref().map(|segments| {
        if segments.len() > 1 {
            segments[..segments.len() - 1].join("/")
        } else if segments.len() == 1 {
            segments[0].to_string()
        } else {
            String::new()
        }
    });

    if url_format == "ssh" || url_format == "https" {
        let parsed =
            normalize_git_owner_repo(git_owner.as_deref().unwrap_or(""), repo_name.unwrap_or(""));
        let Some((owner, repo)) = parsed else {
            return SourceParseResult::Err(CatalogError::invalid_source(format!(
                "Invalid {url_format} repository URL"
            )));
        };
        let host = git_host.clone().unwrap_or_default();
        let normalized_repo = format!("{owner}/{repo}");
        return SourceParseResult::Ok(ParsedSource {
            clone_url_ssh: format!("git@{host}:{owner}/{repo}.git"),
            clone_url_https: format!("https://{host}/{owner}/{repo}.git"),
            host,
            owner,
            repo,
            effective_subpath: explicit_subpath,
            normalized_repo,
        });
    }

    // Shorthand: owner/repo[/subpath...] via /^([^/\s]+)\/([^/\s]+)(?:\/(.+))?$/
    if let Some((owner, repo, tail)) = match_shorthand(raw) {
        let Some((owner, repo)) = normalize_git_owner_repo(owner, repo) else {
            return SourceParseResult::Err(CatalogError::invalid_source(
                "Invalid repository source",
            ));
        };
        let shorthand_subpath = tail
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let effective_subpath = explicit_subpath.or(shorthand_subpath);
        let normalized_repo = format!("{owner}/{repo}");
        return SourceParseResult::Ok(ParsedSource {
            host: GITHUB_HOST.to_string(),
            clone_url_ssh: format!("git@{GITHUB_HOST}:{owner}/{repo}.git"),
            clone_url_https: format!("https://{GITHUB_HOST}/{owner}/{repo}.git"),
            owner,
            repo,
            effective_subpath,
            normalized_repo,
        });
    }

    SourceParseResult::Err(CatalogError::invalid_source(
        "Unsupported repository source format",
    ))
}

/// `/^([^/\s]+)\/([^/\s]+)(?:\/(.+))?$/` — owner and repo are non-empty runs
/// without `/` or whitespace; the tail after the second `/` must be non-empty
/// when the slash is present.
/// 用 splitn(3, '/') 手工实现目标正则：owner/repo 为不含空白与斜杠的
/// 非空段，第三段存在时必须非空（`a/b/` 不匹配）。
fn match_shorthand(raw: &str) -> Option<(&str, &str, Option<&str>)> {
    let mut parts = raw.splitn(3, '/');
    let owner = parts.next().unwrap_or("");
    let repo = parts.next();
    let tail = parts.next();
    if tail == Some("") {
        // `owner/repo/` — the JS regex requires at least one tail character.
        return None;
    }
    let no_ws = |segment: &str| !segment.is_empty() && !segment.contains(char::is_whitespace);
    match (repo, no_ws(owner)) {
        (Some(repo), true) if no_ws(repo) => Some((owner, repo, tail)),
        _ => None,
    }
}

/// source 解析测试：三种格式、`.git` 后缀、subpath 优先级与错误文案。
#[cfg(test)]
mod tests {
    use super::*;

    /// 断言解析成功并返回字段，失败直接 panic。
    fn ok(input: &str, subpath: Option<&str>) -> ParsedSource {
        match parse_skill_repo_source(input, subpath) {
            SourceParseResult::Ok(parsed) => parsed,
            SourceParseResult::Err(error) => panic!("expected ok, got {error:?}"),
        }
    }

    /// 断言解析失败并返回错误消息。
    fn err_message(input: &str) -> String {
        match parse_skill_repo_source(input, None) {
            SourceParseResult::Err(error) => error.message,
            SourceParseResult::Ok(_) => panic!("expected error for {input:?}"),
        }
    }

    /// 行为契约：HTTPS URL 解析出 host/owner/repo 与两条 clone URL。
    #[test]
    fn parses_https_urls() {
        let parsed = ok("https://github.com/anthropics/skills", None);
        assert_eq!(parsed.host, "github.com");
        assert_eq!(parsed.owner, "anthropics");
        assert_eq!(parsed.repo, "skills");
        assert_eq!(parsed.normalized_repo, "anthropics/skills");
        assert_eq!(
            parsed.clone_url_https,
            "https://github.com/anthropics/skills.git"
        );
        assert_eq!(parsed.clone_url_ssh, "git@github.com:anthropics/skills.git");
        assert_eq!(parsed.effective_subpath, None);
    }

    /// 行为契约：多级 owner 与 `.git` 后缀被正确归一化。
    #[test]
    fn parses_https_urls_with_git_suffix_and_extra_segments() {
        let parsed = ok("https://gitlab.com/group/sub/project.git", None);
        assert_eq!(parsed.host, "gitlab.com");
        assert_eq!(parsed.owner, "group/sub");
        assert_eq!(parsed.repo, "project");
        assert_eq!(
            parsed.clone_url_https,
            "https://gitlab.com/group/sub/project.git"
        );
    }

    /// 行为契约：URL 形式的 subpath 只来自显式选项，多余路径段归 owner。
    #[test]
    fn https_subpath_only_from_options() {
        let parsed = ok("https://github.com/a/b", Some("skills"));
        assert_eq!(parsed.effective_subpath.as_deref(), Some("skills"));
        // Extra URL segments are owner components, not a subpath.
        assert_eq!(
            ok("https://github.com/a/b/tree/main", None).effective_subpath,
            None
        );
    }

    /// 行为契约：SSH URL 解析出 host/owner/repo，subpath 仅认显式选项。
    #[test]
    fn parses_ssh_urls() {
        let parsed = ok("git@github.com:anthropics/skills.git", Some("skills"));
        assert_eq!(parsed.host, "github.com");
        assert_eq!(parsed.owner, "anthropics");
        assert_eq!(parsed.repo, "skills");
        assert_eq!(parsed.clone_url_ssh, "git@github.com:anthropics/skills.git");
        assert_eq!(parsed.effective_subpath.as_deref(), Some("skills"));
        assert_eq!(
            ok("git@github.com:anthropics/skills.git", None).effective_subpath,
            None
        );
    }

    /// 行为契约：简写支持可选尾巴 subpath，显式选项优先。
    #[test]
    fn parses_shorthand_with_optional_subpath() {
        let parsed = ok("anthropics/skills", None);
        assert_eq!(parsed.host, "github.com");
        assert_eq!(parsed.effective_subpath, None);

        let with_tail = ok("anthropics/skills/sub/dir", None);
        assert_eq!(with_tail.effective_subpath.as_deref(), Some("sub/dir"));

        let explicit_wins = ok("anthropics/skills/tail", Some("override"));
        assert_eq!(explicit_wins.effective_subpath.as_deref(), Some("override"));
    }

    /// 行为契约：简写的 `.git` 后缀被剥离。
    #[test]
    fn shorthand_strips_git_suffix() {
        let parsed = ok("owner/repo.git", None);
        assert_eq!(parsed.repo, "repo");
        assert_eq!(parsed.normalized_repo, "owner/repo");
    }

    /// 行为契约：空或纯空白源报“源必填”。
    #[test]
    fn rejects_empty_and_missing_sources() {
        assert_eq!(err_message(""), "Repository source is required");
        assert_eq!(err_message("   "), "Repository source is required");
    }

    /// 行为契约：畸形 URL 与无法匹配的输入得到对应错误文案。
    #[test]
    fn rejects_malformed_urls() {
        // https:// with no path segments → invalid https URL.
        assert_eq!(err_message("https://x"), "Invalid https repository URL");
        // git@ with nothing after the colon → invalid ssh URL.
        assert_eq!(err_message("git@"), "Invalid ssh repository URL");
        // No slash at all → falls through to unsupported format.
        assert_eq!(
            err_message("justname"),
            "Unsupported repository source format"
        );
        // Trailing slash with empty tail fails the shorthand regex.
        assert_eq!(err_message("a/b/"), "Unsupported repository source format");
    }

    /// 行为契约：解析失败保留 invalidSource kind。
    #[test]
    fn invalid_kind_is_preserved() {
        match parse_skill_repo_source("", None) {
            SourceParseResult::Err(error) => {
                assert_eq!(error.kind, "invalidSource");
            }
            SourceParseResult::Ok(_) => panic!("expected error"),
        }
    }
}
