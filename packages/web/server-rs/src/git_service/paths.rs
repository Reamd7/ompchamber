//! Path normalization, path-safety checks, and porcelain parsing helpers —
//! port of the module-scope helpers in `server/lib/git/service.js`
//! (`normalizeDirectoryPath`, `validateRepositoryFilePaths`,
//! `cleanBranchName`, `slugWorktreeName`, `parseWorktreePorcelain`,
//! `parseRemoteBranchRef`, …).
//!
//! 中文说明：路径规范化、路径安全检查与 porcelain 输出解析辅助函数，移植自
//! `server/lib/git/service.js` 的模块级 helper（`normalizeDirectoryPath`、
//! `validateRepositoryFilePaths`、`cleanBranchName`、`slugWorktreeName`、
//! `parseWorktreePorcelain`、`parseRemoteBranchRef` 等）。

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

/// JS `normalizeDirectoryPath`: trim, `~` expansion. Non-string input in JS
/// passes through unchanged (then fails `.trim()` checks); here `None` in,
/// `None` out.
/// 对应 JS `normalizeDirectoryPath`：trim 并展开 `~` / `~/` 前缀；入参为
/// `None` 时返回 `None`（JS 中非字符串会原样透传，随后在 `.trim()` 检查处失败）。
pub fn normalize_directory_path(value: Option<&str>) -> Option<String> {
    let value = value?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Some(String::new());
    }
    if trimmed == "~" {
        return home_dir_str();
    }
    if let Some(rest) = trimmed
        .strip_prefix("~/")
        .or_else(|| trimmed.strip_prefix("~\\"))
    {
        return home_dir_str().map(|home| join_str(&home, rest));
    }
    Some(trimmed.to_string())
}

/// home 目录的字符串形式；无法取得时返回 `None`。
fn home_dir_str() -> Option<String> {
    crate::config::home_dir().and_then(|p| p.to_str().map(str::to_string))
}

/// 以路径语义拼接两段字符串并回转为 String（用于 `~/` 展开）。
fn join_str(base: &str, rest: &str) -> String {
    Path::new(base).join(rest).to_string_lossy().to_string()
}

/// 规范化目录输入并要求非空；为空时返回 "Git directory is required" 错误。
pub fn require_directory(value: Option<&str>) -> Result<String, String> {
    let normalized = normalize_directory_path(value).unwrap_or_default();
    if normalized.trim().is_empty() {
        return Err("Git directory is required".to_string());
    }
    Ok(normalized)
}

/// `path.resolve` without symlink resolution.
/// 等价 Node 的 `path.resolve`：转为绝对路径并做词法归一化，不解析符号链接。
pub fn resolve_path(value: impl AsRef<Path>) -> PathBuf {
    absolutize(value.as_ref())
}

/// 相对路径基于当前工作目录补全为绝对路径，再词法归一化；已是绝对路径则只归一化。
pub fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        normalize_path_components(path)
    } else {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        normalize_path_components(&cwd.join(path))
    }
}

/// Lexical `path.normalize`: collapse `.` and `..`, drop trailing separator,
/// keep a leading `//` as-is (POSIX) — mirroring Node's behaviour closely
/// enough for the path-membership checks this module performs.
/// 词法版 `path.normalize`：折叠 `.` 与 `..`、去掉尾部分隔符、POSIX 下保留
/// 前导 `//`、单独处理 Windows 盘符前缀——足够本模块的路径归属判断使用。
pub fn normalize_path_components(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    let mut prefix_root = None;
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => prefix_root = Some(prefix.as_os_str().to_os_string()),
            Component::RootDir => {
                result = PathBuf::from("/");
                prefix_root = None;
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !result.pop() {
                    result.push("..");
                }
            }
            Component::Normal(part) => result.push(part),
        }
    }
    if let Some(prefix) = prefix_root {
        let mut prefixed = PathBuf::from(prefix);
        prefixed.push(result.strip_prefix("/").unwrap_or(Path::new("")));
        return prefixed;
    }
    result
}

/// JS `validateRepositoryFilePaths`: every path must resolve inside (or equal)
/// the repository root — the path-safety check the JS tests pin.
/// 对应 JS `validateRepositoryFilePaths`：每个文件路径拼接到仓库根后必须仍
/// 落在根目录之内（或等于根目录本身），否则返回
/// "Path is outside repository: <path>"——这是 JS 测试固定的路径安全检查。
pub fn validate_repository_file_paths(
    root: impl AsRef<Path>,
    file_paths: &[String],
) -> Result<(), String> {
    let repo_root = absolutize(root.as_ref());
    let root_text = repo_root.to_string_lossy().to_string();
    for file_path in file_paths {
        let absolute = absolutize(&repo_root.join(file_path));
        let text = absolute.to_string_lossy().to_string();
        if text != root_text
            && !text.starts_with(&format!("{}{}", root_text, std::path::MAIN_SEPARATOR))
        {
            return Err(format!("Path is outside repository: {}", file_path));
        }
    }
    Ok(())
}

/// 把 Windows 反斜杠路径转换为 git 使用的正斜杠形式。
pub fn to_git_path(value: &str) -> String {
    value.replace('\\', "/")
}

/// JS `isInsideOrSameDirectory`.
/// 对应 JS `isInsideOrSameDirectory`：target 等于 root 或位于 root 目录之下。
pub fn is_inside_or_same_directory(root: &Path, target: &Path) -> bool {
    match target.strip_prefix(root) {
        Ok(relative) => relative.as_os_str().is_empty() || !relative.starts_with(".."),
        Err(_) => false,
    }
}

/// JS `canonicalPath`: realpath (falling back to the lexical path), then
/// normalize; lowercased on Windows only.
/// 对应 JS `canonicalPath`：先 realpath（失败时回退词法绝对路径）再归一化；
/// 仅 Windows 上把结果转小写。
pub async fn canonical_path(input: impl AsRef<Path>) -> PathBuf {
    let absolute = absolutize(input.as_ref());
    let real = tokio::fs::canonicalize(&absolute).await.unwrap_or(absolute);
    let normalized = normalize_path_components(&real);
    #[cfg(windows)]
    {
        PathBuf::from(normalized.to_string_lossy().to_lowercase())
    }
    #[cfg(not(windows))]
    {
        normalized
    }
}

/// 异步探测路径是否存在（metadata 成功即存在，包含符号链接本身）。
pub async fn check_path_exists(target: impl AsRef<Path>) -> bool {
    tokio::fs::metadata(target.as_ref()).await.is_ok()
}

/// JS `cleanBranchName`: strip `refs/heads/`, `heads/`, then a bare `refs/`.
/// 对应 JS `cleanBranchName`：按顺序剥离 `refs/heads/`、`heads/`、裸 `refs/` 前缀。
pub fn clean_branch_name(branch: &str) -> String {
    if branch.is_empty() {
        return String::new();
    }
    for prefix in ["refs/heads/", "heads/", "refs/"] {
        if let Some(rest) = branch.strip_prefix(prefix) {
            return rest.to_string();
        }
    }
    branch.to_string()
}

/// OpenCode 风格随机命名的形容词词表（编译期常量）。
const OPENCODE_ADJECTIVES: &[&str] = &[
    "brave", "calm", "clever", "cosmic", "crisp", "curious", "eager", "gentle", "glowing", "happy",
    "hidden", "jolly", "kind", "lucky", "mighty", "misty", "neon", "nimble", "playful", "proud",
    "quick", "quiet", "shiny", "silent", "stellar", "sunny", "swift", "tidy", "witty",
];

/// OpenCode 风格随机命名的名词词表（编译期常量）。
const OPENCODE_NOUNS: &[&str] = &[
    "cabin", "cactus", "canyon", "circuit", "comet", "eagle", "engine", "falcon", "forest",
    "garden", "harbor", "island", "knight", "lagoon", "meadow", "moon", "mountain", "nebula",
    "orchid", "otter", "panda", "pixel", "planet", "river", "rocket", "sailor", "squid", "star",
    "tiger", "wizard", "wolf",
];

/// 生成唯一 worktree 名称时的最大尝试次数。
pub const OPENCODE_WORKTREE_ATTEMPTS: usize = 26;

/// 从常量词表中随机取一个词；借 `Box::leak` 把返回值提升为 `&'static str`
/// （两个词表均为编译期常量，不会无限泄漏）。
pub fn pick_random(values: &[&str]) -> &'static str {
    use rand::Rng;
    let mut rng = rand::rng();
    let picked: &str = values[rng.random_range(0..values.len())];
    // The two source lists are compile-time constants.
    Box::leak(picked.to_string().into_boxed_str())
}

/// 生成 `<形容词>-<名词>` 形式的随机名，用于自动命名的 worktree。
pub fn generate_opencode_random_name() -> String {
    format!(
        "{}-{}",
        pick_random(OPENCODE_ADJECTIVES),
        pick_random(OPENCODE_NOUNS)
    )
}

/// JS `slugWorktreeName` — slugify pipeline, capped at 80 chars.
/// 对应 JS `slugWorktreeName`：先剥 refs 前缀，再依次做空白折叠转连字符、
/// 去首尾斜杠、斜杠替换、非法字符替换、折叠连续连字符、去首尾连字符，
/// 最后截断到 80 个字符。
pub fn slug_worktree_name(value: &str) -> String {
    let mut out = value.trim().to_string();
    for prefix in ["refs/heads/", "heads/"] {
        if let Some(rest) = out.strip_prefix(prefix) {
            out = rest.to_string();
            break;
        }
    }
    // `\s+` → `-`
    let mut out = split_ws_hyphenate(&out);
    out = trim_slashes(&out);
    out = out.replace('/', "-");
    out = replace_non_slug_chars(&out);
    out = collapse_hyphens(&out);
    out = out.trim_matches('-').to_string();
    out.chars().take(80).collect()
}

/// 把连续空白折叠为单个 `-`（对应 JS 的 `\s+` → `-` 替换）。
fn split_ws_hyphenate(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut in_ws = false;
    for ch in value.chars() {
        if ch.is_whitespace() {
            if !in_ws {
                result.push('-');
            }
            in_ws = true;
        } else {
            in_ws = false;
            result.push(ch);
        }
    }
    result
}

/// 去掉字符串首尾的 `/`。
fn trim_slashes(value: &str) -> String {
    value.trim_matches('/').to_string()
}

/// 把非 slug 合法字符（字母数字、`.`、`_`、`-` 之外）替换为 `-`。
fn replace_non_slug_chars(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || ch == '.' || ch == '_' || ch == '-' {
            result.push(ch);
        } else {
            result.push('-');
        }
    }
    result
}

/// 把连续多个 `-` 折叠为一个。
fn collapse_hyphens(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut previous_hyphen = false;
    for ch in value.chars() {
        let is_hyphen = ch == '-';
        if is_hyphen && previous_hyphen {
            continue;
        }
        previous_hyphen = is_hyphen;
        result.push(ch);
    }
    result
}

/// One parsed `git worktree list --porcelain` entry (JS `parseWorktreePorcelain`).
/// `git worktree list --porcelain` 输出的单条解析结果（对应 JS
/// `parseWorktreePorcelain`）。
#[derive(Debug, Clone, Default)]
pub struct WorktreePorcelainEntry {
    /// 工作树的绝对路径（`worktree <path>` 行）。
    pub worktree: String,
    /// HEAD 提交哈希（`HEAD <sha>` 行）。
    pub head: String,
    /// 完整分支引用（`branch <ref>` 行）；detached HEAD 时为空字符串。
    pub branch_ref: String,
    /// 去掉 `refs/heads/` 前缀的短分支名；detached HEAD 时为空字符串。
    pub branch: String,
}

/// 解析 `git worktree list --porcelain` 输出：`worktree ` 行开启新条目，
/// `HEAD `/`branch ` 行填充当前条目字段，空行或输入结束收束条目；
/// 跳过没有 worktree 路径的残缺条目。
pub fn parse_worktree_porcelain(raw: &str) -> Vec<WorktreePorcelainEntry> {
    let mut entries: Vec<WorktreePorcelainEntry> = Vec::new();
    let mut current: Option<WorktreePorcelainEntry> = None;

    for line in raw.split('\n') {
        let line = line.trim();
        if line.is_empty() {
            if let Some(entry) = current.take()
                && !entry.worktree.is_empty()
            {
                entries.push(entry);
            }
            continue;
        }
        if let Some(worktree) = line.strip_prefix("worktree ") {
            if let Some(entry) = current.take()
                && !entry.worktree.is_empty()
            {
                entries.push(entry);
            }
            current = Some(WorktreePorcelainEntry {
                worktree: worktree.trim().to_string(),
                ..Default::default()
            });
            continue;
        }
        let Some(entry) = current.as_mut() else {
            continue;
        };
        if let Some(head) = line.strip_prefix("HEAD ") {
            entry.head = head.trim().to_string();
        } else if let Some(branch_ref) = line.strip_prefix("branch ") {
            let branch_ref = branch_ref.trim().to_string();
            entry.branch = clean_branch_name(&branch_ref);
            entry.branch_ref = branch_ref;
        }
    }

    if let Some(entry) = current.take()
        && !entry.worktree.is_empty()
    {
        entries.push(entry);
    }
    entries
}

/// 规范化起始 ref：入参空白或缺失时回退为 "HEAD"，否则返回 trim 后的值。
pub fn normalize_start_ref(value: Option<&str>) -> String {
    let trimmed = value.unwrap_or("").trim();
    if trimmed.is_empty() {
        "HEAD".to_string()
    } else {
        trimmed.to_string()
    }
}

/// 校验缩写或完整 commit 哈希：长度 7..=40 且全为十六进制字符。
pub fn is_valid_commit_hash(hash: &str) -> bool {
    hash.len() >= 7 && hash.len() <= 40 && hash.chars().all(|c| c.is_ascii_hexdigit())
}

/// JS `parseRemoteBranchRef` → `(remote, branch, remoteRef, fullRef)`.
/// 对应 JS `parseRemoteBranchRef` 的解析结果四元组
/// `(remote, branch, remoteRef, fullRef)`。
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteBranchRef {
    /// 远端名（如 `origin`）。
    pub remote: String,
    /// 远端上的分支名（不含远端前缀）。
    pub branch: String,
    /// 短形式远端引用（如 `origin/main`）。
    pub remote_ref: String,
    /// 完整引用（如 `refs/remotes/origin/main`）。
    pub full_ref: String,
}

/// 解析远端分支引用：接受 `refs/remotes/<remote>/<branch>`、`remotes/...`
/// （递归补上 `refs/` 前缀）与裸 `<remote>/<branch>` 三种形态；缺少分隔符
/// 或远端/分支为空时返回 `None`。
pub fn parse_remote_branch_ref(value: &str) -> Option<RemoteBranchRef> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(rest) = trimmed.strip_prefix("refs/remotes/") {
        let slash_index = rest.find('/');
        let invalid =
            |idx: Option<usize>| idx.is_none() || Some(rest.len() - 1) == idx || idx == Some(0);
        let slash_index = match slash_index {
            Some(idx) if !invalid(Some(idx)) => idx,
            _ => return None,
        };
        return Some(RemoteBranchRef {
            remote: rest[..slash_index].to_string(),
            branch: rest[slash_index + 1..].to_string(),
            remote_ref: rest.to_string(),
            full_ref: format!("refs/remotes/{}", rest),
        });
    }

    if trimmed.starts_with("remotes/") {
        return parse_remote_branch_ref(&format!("refs/{}", trimmed));
    }

    let slash_index = match trimmed.find('/') {
        Some(idx) if idx > 0 && idx != trimmed.len() - 1 => idx,
        _ => return None,
    };

    Some(RemoteBranchRef {
        remote: trimmed[..slash_index].to_string(),
        branch: trimmed[slash_index + 1..].to_string(),
        remote_ref: trimmed.to_string(),
        full_ref: format!("refs/remotes/{}", trimmed),
    })
}

/// JS `normalizeFilePathList`: trim, drop empties, dedupe preserving order.
/// 对应 JS `normalizeFilePathList`：逐项 trim、丢弃空值，并按首次出现顺序去重。
pub fn normalize_file_path_list(paths: &[serde_json::Value]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for value in paths {
        let text = value.as_str().map(str::trim).unwrap_or("").to_string();
        if text.is_empty() || !seen.insert(text.clone()) {
            continue;
        }
        out.push(text);
    }
    out
}

/// JS `getOpenCodeDataPath`: `$XDG_DATA_HOME/opencode` or
/// `~/.local/share/opencode`.
/// 对应 JS `getOpenCodeDataPath`：优先 `$XDG_DATA_HOME/opencode`，
/// 否则 `~/.local/share/opencode`。
pub fn opencode_data_path() -> PathBuf {
    let xdg = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let base = match xdg {
        Some(dir) => PathBuf::from(dir),
        None => crate::config::home_dir()
            .unwrap_or_default()
            .join(".local")
            .join("share"),
    };
    base.join("opencode")
}

/// JS `buildSshCommand`/`escapeSshKeyPath` — SSH key path escaped for
/// `core.sshCommand`. Dangerous characters reject; Unix single-quote-escapes.
/// 对应 JS `escapeSshKeyPath`：为 `core.sshCommand` 转义 SSH key 路径——
/// 含危险字符的路径直接报错；Unix 用单引号包裹并转义内部单引号；Windows
/// 先把 `C:/` 盘符形式改写为 `/c/` 再单引号包裹。
pub fn escape_ssh_key_path(ssh_key_path: &str) -> Result<String, String> {
    let is_windows = cfg!(windows);
    let mut normalized = ssh_key_path.to_string();
    if is_windows {
        normalized = normalized.replace('\\', "/");
    }
    let dangerous: &[char] = &[
        '`', '$', '!', '"', '\'', ';', '&', '|', '<', '>', '(', ')', '{', '}', '[', ']', '*', '?',
        '#', '~',
    ];
    if normalized.chars().any(|c| dangerous.contains(&c)) {
        return Err(format!(
            "SSH key path contains invalid characters: {}",
            ssh_key_path
        ));
    }
    if is_windows {
        let mut unix_path = normalized;
        let bytes = unix_path.as_bytes();
        if bytes.len() >= 3
            && bytes[1] == b':'
            && bytes[2] == b'/'
            && bytes[0].is_ascii_alphabetic()
        {
            let drive = (bytes[0] as char).to_ascii_lowercase();
            unix_path = format!("/{}{}", drive, &unix_path[2..]);
        }
        Ok(format!("'{}'", unix_path))
    } else {
        let escaped = normalized.replace('\'', "'\\''");
        Ok(format!("'{}'", escaped))
    }
}

/// 对应 JS `buildSshCommand`：生成 `ssh -i '<key>' -o IdentitiesOnly=yes`；
/// key 路径非法时透传转义错误。
pub fn build_ssh_command(ssh_key_path: &str) -> Result<String, String> {
    let escaped = escape_ssh_key_path(ssh_key_path)?;
    Ok(format!("ssh -i {} -o IdentitiesOnly=yes", escaped))
}

/// 按扩展名推断图片 MIME 类型；未知扩展名回退 `application/octet-stream`。
pub fn image_mime_type(file_path: &str) -> &'static str {
    let ext = file_path.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "bmp" => "image/bmp",
        "avif" => "image/avif",
        _ => "application/octet-stream",
    }
}

/// 判断扩展名是否属于支持的图片格式（大小写不敏感）。
pub fn is_image_file(file_path: &str) -> bool {
    let ext = file_path.rsplit('.').next().unwrap_or("").to_lowercase();
    matches!(
        ext.as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "ico" | "bmp" | "avif"
    )
}

/// 按行拆分 git 输出：逐行 trim 并丢弃空行。
pub fn trim_git_lines(value: &str) -> Vec<String> {
    value
        .split('\n')
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// 路径规范化与纯解析函数的回归测试。
#[cfg(test)]
mod tests {
    use super::*;

    /// 验证 `normalize_directory_path` 的 `~` 展开与空白 trim 行为。
    #[test]
    fn normalize_directory_path_expands_tilde() {
        let home = crate::config::home_dir()
            .expect("home")
            .to_string_lossy()
            .to_string();
        assert_eq!(
            normalize_directory_path(Some(" ~ ")).as_deref(),
            Some(home.as_str())
        );
        assert_eq!(
            normalize_directory_path(Some("~/repo")),
            Some(join_str(&home, "repo"))
        );
        assert_eq!(normalize_directory_path(Some("  ")), Some(String::new()));
        assert_eq!(
            normalize_directory_path(Some("/plain/path")),
            Some("/plain/path".to_string())
        );
    }

    /// 验证仓库内路径通过校验，`..` 逃逸与绝对路径逃逸被拒绝且错误消息固定。
    #[test]
    fn validate_repository_file_paths_rejects_escape() {
        assert!(
            validate_repository_file_paths(
                "/repo",
                &["ok.txt".to_string(), "sub/a.ts".to_string()]
            )
            .is_ok()
        );
        let err =
            validate_repository_file_paths("/repo", &["../secret.txt".to_string()]).unwrap_err();
        assert_eq!(err, "Path is outside repository: ../secret.txt");
        assert!(validate_repository_file_paths("/repo", &["/etc/passwd".to_string()]).is_err());
    }

    /// 验证 `clean_branch_name` 对各类 refs 前缀的剥离顺序。
    #[test]
    fn clean_branch_name_strips_prefixes() {
        assert_eq!(clean_branch_name("refs/heads/main"), "main");
        assert_eq!(clean_branch_name("heads/main"), "main");
        assert_eq!(
            clean_branch_name("refs/remotes/origin/main"),
            "remotes/origin/main"
        );
        assert_eq!(clean_branch_name(""), "");
        assert_eq!(clean_branch_name("feature/x"), "feature/x");
    }

    /// 验证 `slug_worktree_name` 流水线：空白/斜杠/非法字符处理、连字符折叠
    /// 与 80 字符截断。
    #[test]
    fn slug_worktree_name_pipeline() {
        assert_eq!(slug_worktree_name("  My Cool Feature  "), "My-Cool-Feature");
        assert_eq!(slug_worktree_name("refs/heads/wt"), "wt");
        assert_eq!(slug_worktree_name("a/b/c"), "a-b-c");
        assert_eq!(slug_worktree_name("--weird--"), "weird");
        assert_eq!(slug_worktree_name("a  --  b"), "a-b");
        let long = "x".repeat(200);
        assert_eq!(slug_worktree_name(&long).len(), 80);
        assert_eq!(slug_worktree_name("/lead/trail/"), "lead-trail");
    }

    /// 验证 porcelain 输出解析出多个条目，detached 条目的 branch 字段为空。
    #[test]
    fn parse_worktree_porcelain_entries() {
        let raw = "worktree /repo\nHEAD abc123\nbranch refs/heads/main\n\nworktree /wt\nHEAD def456\ndetached\n\n";
        let entries = parse_worktree_porcelain(raw);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].worktree, "/repo");
        assert_eq!(entries[0].head, "abc123");
        assert_eq!(entries[0].branch, "main");
        assert_eq!(entries[0].branch_ref, "refs/heads/main");
        assert_eq!(entries[1].branch, "");
    }

    /// 验证远端分支引用的三种输入形态，以及空值、无分隔符、首尾分隔符等
    /// 非法输入返回 `None`。
    #[test]
    fn parse_remote_branch_ref_variants() {
        let parsed = parse_remote_branch_ref("origin/main").unwrap();
        assert_eq!(parsed.remote, "origin");
        assert_eq!(parsed.branch, "main");
        assert_eq!(parsed.remote_ref, "origin/main");
        assert_eq!(parsed.full_ref, "refs/remotes/origin/main");

        let parsed = parse_remote_branch_ref("remotes/origin/react").unwrap();
        assert_eq!(parsed.remote_ref, "origin/react");

        let parsed = parse_remote_branch_ref("refs/remotes/pr-owner/head").unwrap();
        assert_eq!(parsed.remote, "pr-owner");
        assert_eq!(parsed.branch, "head");

        assert!(parse_remote_branch_ref("").is_none());
        assert!(parse_remote_branch_ref("noseparator").is_none());
        assert!(parse_remote_branch_ref("origin/").is_none());
        assert!(parse_remote_branch_ref("/origin").is_none());
    }

    /// 验证 commit 哈希校验拒绝过短、非十六进制与超长输入。
    #[test]
    fn commit_hash_validation() {
        assert!(is_valid_commit_hash("1234567890abcdef"));
        assert!(is_valid_commit_hash(
            "1234567890abcdef1234567890abcdef12345678"
        ));
        assert!(!is_valid_commit_hash("123456"));
        assert!(!is_valid_commit_hash("HEAD"));
        assert!(!is_valid_commit_hash("--hard"));
        assert!(!is_valid_commit_hash(
            "1234567890abcdef1234567890abcdef123456789"
        ));
    }

    /// 验证 SSH key 路径转义：普通路径单引号包裹、危险字符直接拒绝，
    /// 以及 `build_ssh_command` 的输出形状。
    #[test]
    fn ssh_key_escaping() {
        assert_eq!(
            escape_ssh_key_path("/home/me/.ssh/id_ed25519").unwrap(),
            "'/home/me/.ssh/id_ed25519'"
        );
        assert_eq!(
            escape_ssh_key_path("/home/me/my key").unwrap(),
            "'/home/me/my key'"
        );
        // JS `dangerousChars` includes the single quote, so such paths are
        // rejected outright (the unix quote-escaping branch never sees them).
        assert!(escape_ssh_key_path("/home/o'neal/key").is_err());
        assert!(escape_ssh_key_path("/path;rm -rf").is_err());
        assert_eq!(
            build_ssh_command("/key").unwrap(),
            "ssh -i '/key' -o IdentitiesOnly=yes"
        );
    }

    /// 验证图片扩展名判断与 MIME 推断对大小写不敏感。
    #[test]
    fn image_helpers() {
        assert!(is_image_file("logo.PNG"));
        assert!(!is_image_file("main.rs"));
        assert_eq!(image_mime_type("a.jpg"), "image/jpeg");
        assert_eq!(image_mime_type("a.svg"), "image/svg+xml");
    }

    /// 验证目录归属判断：等于根或在根之下为真，同前缀的兄弟目录与外部路径为假。
    #[test]
    fn is_inside_or_same_directory_checks() {
        let root = Path::new("/worktrees");
        assert!(is_inside_or_same_directory(root, Path::new("/worktrees")));
        assert!(is_inside_or_same_directory(
            root,
            Path::new("/worktrees/wt/sub")
        ));
        assert!(!is_inside_or_same_directory(
            root,
            Path::new("/worktrees-other")
        ));
        assert!(!is_inside_or_same_directory(root, Path::new("/elsewhere")));
    }
}
