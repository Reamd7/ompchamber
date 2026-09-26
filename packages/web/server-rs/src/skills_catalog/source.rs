//! Port of `server/lib/skills-catalog/source.js` (`parseSkillRepoSource`):
//! parse a repository source string (HTTPS URL, SSH URL, or
//! `owner/repo[/subpath]` shorthand) into clone URLs plus the normalized
//! `owner/repo` identifier. The JS implementation is literal string surgery;
//! this port mirrors it branch for branch.

use crate::skills_catalog::error::CatalogError;

const GITHUB_HOST: &str = "github.com";

/// Successful `parseSkillRepoSource` payload (`ok: true` fields).
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedSource {
    pub host: String,
    pub owner: String,
    pub repo: String,
    pub clone_url_ssh: String,
    pub clone_url_https: String,
    /// Subpath from `options.subpath` (SSH/HTTPS URLs) or from either the
    /// explicit option or the shorthand tail; `None` when absent.
    pub effective_subpath: Option<String>,
    pub normalized_repo: String,
}

/// Result of `parseSkillRepoSource`: parsed fields or an `invalidSource`
/// error carrying the exact JS message.
#[derive(Debug, Clone, PartialEq)]
pub enum SourceParseResult {
    Ok(ParsedSource),
    Err(CatalogError),
}

impl SourceParseResult {
    pub fn is_ok(&self) -> bool {
        matches!(self, SourceParseResult::Ok(_))
    }
}

/// JS `normalizeGitOwnerRepo`: trims, strips a trailing `.git` (case
/// insensitive) from the repo, and rejects empty owner/repo.
fn normalize_git_owner_repo(owner: &str, repo: &str) -> Option<(String, String)> {
    let owner = owner.trim();
    let repo = repo.trim();
    let repo = strip_dot_git(repo);
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner.to_string(), repo.to_string()))
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(input: &str, subpath: Option<&str>) -> ParsedSource {
        match parse_skill_repo_source(input, subpath) {
            SourceParseResult::Ok(parsed) => parsed,
            SourceParseResult::Err(error) => panic!("expected ok, got {error:?}"),
        }
    }

    fn err_message(input: &str) -> String {
        match parse_skill_repo_source(input, None) {
            SourceParseResult::Err(error) => error.message,
            SourceParseResult::Ok(_) => panic!("expected error for {input:?}"),
        }
    }

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

    #[test]
    fn shorthand_strips_git_suffix() {
        let parsed = ok("owner/repo.git", None);
        assert_eq!(parsed.repo, "repo");
        assert_eq!(parsed.normalized_repo, "owner/repo");
    }

    #[test]
    fn rejects_empty_and_missing_sources() {
        assert_eq!(err_message(""), "Repository source is required");
        assert_eq!(err_message("   "), "Repository source is required");
    }

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
