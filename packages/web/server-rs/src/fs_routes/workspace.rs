//! Workspace boundary enforcement — port of routes.js
//! `resolveWorkspacePath` / `resolveWorkspacePathFromContext` /
//! `resolveWorkspacePathFromWorktrees` / `resolveReadPathFromContext` plus
//! the outside-file grant store and an inline port of the injected
//! `resolveProjectDirectory` (opencode/project-directory-runtime.js).
//!
//! 中文说明：工作区边界执行层，回答"客户端请求的路径是否允许访问"。
//! 准入规则：活动项目根与用户配置根 `~/.config/ompchamber` 恒准入；
//! 项目根本身是 symlink 时回退用客户端原始目录重试；最后再尝试
//! `git worktree list` 列出的各 worktree 根。读类路由还可用一次性
//! 外部文件授权（grant token）精确豁免工作区外的单个文件。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::HeaderMap;

use super::paths::{
    is_path_within_root, normalize_directory_path, realpath, resolve_path, user_config_root,
};

/// 外部文件授权的有效期（10 分钟），过期条目由 `prune` 清除。
pub const OUTSIDE_FILE_GRANT_TTL_MS: u64 = 10 * 60 * 1000;

/// 路径不在任何准入根之内时的统一错误文案（映射为 400）。
const ERR_OUTSIDE_WORKSPACE: &str = "Path is outside of active workspace";

/// Result shape of the JS `resolveWorkspacePath*` helpers. `granted` is only
/// true for outside-workspace reads riding an exact-path grant.
///
/// 中文说明：路径解析成功的返回形状。`resolved` 是词法解析后的绝对
/// 路径（未做 canonicalize），`base` 是使其获得准入的根。
#[derive(Debug, Clone)]
pub struct WorkspacePath {
    /// 使该路径获得准入的根（项目根 / 配置根 / worktree 根 / 授权基目录）。
    pub base: PathBuf,
    /// 词法解析后的目标绝对路径（不跟随 symlink）。
    pub resolved: PathBuf,
    /// 仅当通过外部文件授权（精确路径匹配）读取时为 true。
    pub granted: bool,
}

/// 活动项目目录的解析结果：header/query 候选与 settings 回退链的产出。
#[derive(Debug, Default)]
pub struct ProjectDirectory {
    /// 校验通过的 canonical 项目目录（realpath 后）；失败时为 None。
    pub directory: Option<PathBuf>,
    /// 客户端原始请求的词法目录（与 canonical 不同时用于 symlink 重试）。
    pub requested_directory: Option<PathBuf>,
    /// 全部候选均失败时保留最后一个错误文案。
    pub error: Option<String>,
}

/// 单条外部文件授权的存储条目（token 对应的授权内容）。
#[derive(Debug, Clone)]
struct OutsideGrant {
    /// 被授权文件的 canonical 路径（realpath 后，精确匹配）。
    canonical_path: PathBuf,
    /// 授权文件所在目录，作为返回 `WorkspacePath` 的 base。
    base: PathBuf,
    /// 允许的操作集合（如 "read"）；为空时铸造方默认注入 "read"。
    scopes: HashSet<String>,
    /// 授权过期时刻（Unix 毫秒）。
    expires_at: u64,
}

/// 外部文件授权存储：token → 授权条目，带 TTL 清扫；跨模块共享
/// （`FsState::grants` 暴露同一实例）。
#[derive(Default)]
pub struct OutsideGrantStore {
    /// token → 授权映射；锁中毒以 `into_inner` 恢复。
    grants: Mutex<HashMap<String, OutsideGrant>>,
}

/// 当前 Unix 时间戳（毫秒）；时钟早于 epoch 时返回 0 而非 panic。
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 授权的铸造、清扫与校验。
impl OutsideGrantStore {
    /// 删除已过期的授权条目（每次 mint/resolve 前惰性执行）。
    fn prune(&self) {
        let mut grants = self.grants.lock().unwrap_or_else(|e| e.into_inner());
        let now = now_ms();
        grants.retain(|_, grant| grant.expires_at > now);
    }

    /// routes.js `mintOutsideFileGrant`. Cross-module callers (feature
    /// routes mint grants after an explicit user file pick) use this too.
    ///
    /// 中文说明：为单个工作区外文件铸造授权。路径必须可 realpath 且
    /// 是普通文件；scopes 去空白去重，为空时默认 ["read"]。返回 JS
    /// 形状的 { path, outsideFileGrant, expiresAt }，供客户端后续
    /// 携带 token 读取该文件。
    pub async fn mint(
        &self,
        target_path: &str,
        scopes: &[&str],
    ) -> Result<serde_json::Value, String> {
        let raw = target_path.trim();
        if raw.is_empty() {
            return Err("Path is required".to_string());
        }
        let canonical_path = realpath(Path::new(raw)).map_err(|e| e.to_string())?;
        let stats = tokio::fs::metadata(&canonical_path)
            .await
            .map_err(|e| e.to_string())?;
        if !stats.is_file() {
            return Err("Outside file grants require a file path".to_string());
        }
        self.prune();
        let token = super::paths::random_uuid();
        let mut normalized_scopes: HashSet<String> = scopes
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if normalized_scopes.is_empty() {
            normalized_scopes.insert("read".to_string());
        }
        let base = super::paths::dirname(&canonical_path);
        let expires_at = now_ms() + OUTSIDE_FILE_GRANT_TTL_MS;
        self.grants
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                token.clone(),
                OutsideGrant {
                    canonical_path: canonical_path.clone(),
                    base: base.clone(),
                    scopes: normalized_scopes,
                    expires_at,
                },
            );
        Ok(serde_json::json!({
            "path": canonical_path.to_string_lossy(),
            "outsideFileGrant": token,
            "expiresAt": expires_at,
        }))
    }

    /// routes.js `resolveOutsideFileGrant`.
    ///
    /// 中文说明：校验授权 token 并精确匹配目标路径：token 缺失/无效/
    /// 过期、scope 不符、或请求路径的 canonical 形式与授权路径不同，
    /// 均拒绝；通过时返回 granted=true 的 `WorkspacePath`。
    pub async fn resolve(
        &self,
        token: Option<&str>,
        target_path: &Path,
        scope: &str,
    ) -> Result<WorkspacePath, GrantError> {
        self.prune();
        let token = token.map(str::trim).filter(|t| !t.is_empty());
        let Some(token) = token else {
            return Err(GrantError::Denied(
                "Outside workspace file access requires a grant".to_string(),
            ));
        };
        let grants = self.grants.lock().unwrap_or_else(|e| e.into_inner());
        let Some(grant) = grants.get(token) else {
            return Err(GrantError::Denied(
                "Outside workspace file grant is invalid or expired".to_string(),
            ));
        };
        if !grant.scopes.contains(scope) {
            return Err(GrantError::Denied(
                "Outside workspace file grant does not allow this operation".to_string(),
            ));
        }
        let canonical_path = realpath(target_path).map_err(GrantError::Io)?;
        if canonical_path != grant.canonical_path {
            return Err(GrantError::Denied(
                "Outside workspace file grant does not match requested path".to_string(),
            ));
        }
        Ok(WorkspacePath {
            base: grant.base.clone(),
            resolved: canonical_path,
            granted: true,
        })
    }
}

/// 授权校验失败的两种出口：业务拒绝（400 文案）与 realpath IO 错误。
#[derive(Debug)]
pub enum GrantError {
    /// 400 with the JS message (`{ error: ... }`).
    ///
    /// 中文说明：业务性拒绝，携带 JS 原样的 `{ error: ... }` 文案。
    Denied(String),
    /// realpath failure — maps through the route's generic io error handling.
    ///
    /// 中文说明：realpath 失败，走路由的通用 io 错误处理路径。
    Io(std::io::Error),
}

/// routes.js `resolveWorkspacePath`: the active project root and the user
/// config root (`~/.config/ompchamber`) are the two always-admitted bases.
///
/// 中文说明：词法准入判定——目标路径在 `base_directory`（缺省回落
/// home 目录）或用户配置根之内即放行，`granted` 恒为 false；两个根
/// 都不命中则返回统一的工作区外错误。
pub fn resolve_workspace_path(
    target_path: &str,
    base_directory: Option<&Path>,
) -> Result<WorkspacePath, String> {
    let normalized = normalize_directory_path(target_path);
    if normalized.is_empty() {
        return Err("Path is required".to_string());
    }
    let resolved = resolve_path(&normalized);
    let base = match base_directory {
        Some(dir) => resolve_path(&dir.to_string_lossy()),
        None => resolve_path(
            &crate::config::home_dir()
                .unwrap_or_default()
                .to_string_lossy(),
        ),
    };
    if is_path_within_root(&resolved, &base) {
        return Ok(WorkspacePath {
            base,
            resolved,
            granted: false,
        });
    }
    let config_root = resolve_path(&user_config_root().to_string_lossy());
    if is_path_within_root(&resolved, &config_root) {
        return Ok(WorkspacePath {
            base: config_root,
            resolved,
            granted: false,
        });
    }
    Err(ERR_OUTSIDE_WORKSPACE.to_string())
}

/// routes.js `resolveWorkspacePathFromWorktrees`: last-chance fallback that
/// admits paths inside any `git worktree list` entry of the active project.
///
/// 中文说明：兜底准入——目标路径落在活动项目任一 `git worktree`
/// 根之内时放行；worktree 枚举失败视为空列表（与 JS 一致）。
async fn resolve_workspace_path_from_worktrees(
    target_path: &str,
    base_directory: &Path,
) -> Result<WorkspacePath, String> {
    let normalized = normalize_directory_path(target_path);
    if normalized.is_empty() {
        return Err("Path is required".to_string());
    }
    let resolved = resolve_path(&normalized);
    let resolved_base = resolve_path(&base_directory.to_string_lossy());

    for worktree in list_worktree_roots(&resolved_base).await {
        let candidate = resolve_path(&worktree.to_string_lossy());
        if is_path_within_root(&resolved, &candidate) {
            return Ok(WorkspacePath {
                base: candidate,
                resolved,
                granted: false,
            });
        }
    }
    Err(ERR_OUTSIDE_WORKSPACE.to_string())
}

/// Minimal inline port of git/service.js `getWorktrees`: spawn
/// `git worktree list --porcelain` in `directory` and collect the
/// `worktree <path>` lines. Any failure yields an empty list (the JS treats
/// non-repositories as authoritatively empty).
///
/// 中文说明：在 `directory` 中运行 `git worktree list --porcelain`，
/// 收集 `worktree <path>` 行；任何失败（spawn 错误、非零退出、非
/// 仓库）都返回空列表。
async fn list_worktree_roots(directory: &Path) -> Vec<PathBuf> {
    let output = run_worktree_list(directory).await;
    let stdout = match output {
        Ok(stdout) => stdout,
        Err(_) => return Vec::new(),
    };
    stdout
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .filter(|path| !path.trim().is_empty())
        .map(|path| PathBuf::from(path.trim()))
        .collect()
}

/// 实际执行 `git worktree list --porcelain` 子进程并返回 stdout；
/// 退出码非零时转为 io 错误。
async fn run_worktree_list(directory: &Path) -> Result<String, std::io::Error> {
    let output = tokio::process::Command::new(git_binary())
        .arg("worktree")
        .arg("list")
        .arg("--porcelain")
        .current_dir(directory)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await?;
    if !output.status.success() {
        return Err(std::io::Error::other("git worktree list failed"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// env-runtime.js `resolveGitBinaryForSpawn` — plain `git` off POSIX.
///
/// 中文说明：返回用于 spawn 的 git 可执行文件名；POSIX/Windows 统一
/// 用 PATH 上的裸 `git`。
pub fn git_binary() -> &'static str {
    "git"
}

/// routes.js `resolveWorkspacePathFromContext`: project root first, then the
/// raw (lexical) directory the client asked for when the project root itself
/// is a symlink, then worktree roots.
///
/// 中文说明：三级准入链——① 项目根；② 项目根是 symlink 且客户端
/// 原始目录不同时，用原始目录做词法重试；③ worktree 根。仅当上一级
/// 返回"工作区外"错误才尝试下一级，其它错误（如路径为空）直接透传。
pub async fn resolve_workspace_path_from_context(
    headers: &HeaderMap,
    query: &HashMap<String, String>,
    target_path: &str,
) -> Result<WorkspacePath, String> {
    let project = resolve_project_directory(headers, query).await;
    let Some(directory) = project.directory.clone() else {
        return Err(project
            .error
            .unwrap_or_else(|| "Active workspace is required".to_string()));
    };

    let resolved = resolve_workspace_path(target_path, Some(&directory));
    match resolved {
        Ok(path) => Ok(path),
        Err(error) if error != ERR_OUTSIDE_WORKSPACE => Err(error),
        Err(_) => {
            // The validated project directory is canonical; retry against the
            // raw directory the client requested so file-tree paths under a
            // symlinked project root stay addressable.
            let requested_base = project.requested_directory.clone();
            if let Some(requested_base) = requested_base
                && requested_base != directory
            {
                let lexical = resolve_workspace_path(target_path, Some(&requested_base));
                if let Ok(path) = lexical {
                    return Ok(path);
                }
            }
            resolve_workspace_path_from_worktrees(target_path, &directory).await
        }
    }
}

/// routes.js `resolveReadPathFromContext`: read-family routes may instead
/// ride an exact-path outside-file grant when `allowOutsideWorkspace=true`.
///
/// 中文说明：读类路由的路径解析——query 带
/// `allowOutsideWorkspace=true` 时改走外部文件授权（要求
/// `outsideFileGrant` token 精确匹配）；否则回落常规工作区准入。
pub async fn resolve_read_path_from_context(
    grants: &OutsideGrantStore,
    headers: &HeaderMap,
    query: &HashMap<String, String>,
    target_path: &str,
    scope: &str,
) -> Result<WorkspacePath, ReadPathError> {
    if query.get("allowOutsideWorkspace").map(String::as_str) == Some("true") {
        let normalized = normalize_directory_path(target_path);
        if normalized.is_empty() {
            return Err(ReadPathError::Denied("Path is required".to_string()));
        }
        let resolved = resolve_path(&normalized);
        return grants
            .resolve(
                query.get("outsideFileGrant").map(String::as_str),
                &resolved,
                scope,
            )
            .await
            .map_err(ReadPathError::from);
    }
    resolve_workspace_path_from_context(headers, query, target_path)
        .await
        .map_err(ReadPathError::Denied)
}

/// 读路径解析的错误出口：与 `GrantError` 同构，Denied 携带 400 文案。
#[derive(Debug)]
pub enum ReadPathError {
    /// 业务性拒绝（路径必填、授权缺失/无效、工作区外等）。
    Denied(String),
    /// realpath 失败，走路由的通用 io 错误处理。
    Io(std::io::Error),
}

/// 把授权错误透传为读路径错误（不改变语义，仅统一类型）。
impl From<GrantError> for ReadPathError {
    /// 逐变体映射 `GrantError` → `ReadPathError`。
    fn from(error: GrantError) -> Self {
        match error {
            GrantError::Denied(message) => ReadPathError::Denied(message),
            GrantError::Io(error) => ReadPathError::Io(error),
        }
    }
}

/// Inline port of opencode/project-directory-runtime.js
/// `resolveProjectDirectory`: `x-opencode-directory` header (decoded when
/// `x-opencode-directory-encoding: uri`) and the `directory` query parameter
/// win; otherwise fall back to settings (`lastDirectory`, then the active
/// project) read straight from `~/.config/ompchamber/settings.json`.
///
/// 中文说明：解析活动项目目录。优先级：`x-opencode-directory` header
/// （编码为 uri 时先解码）与 `directory` query 参数（依次尝试，首个
/// 通过校验者胜出）；否则回落 settings.json 的 `lastDirectory`，再
/// 回落 `activeProjectId` 对应项目（无激活 id 时取列表首项）。
pub async fn resolve_project_directory(
    headers: &HeaderMap,
    query: &HashMap<String, String>,
) -> ProjectDirectory {
    let mut candidates: Vec<String> = Vec::new();

    let header_encoding = headers
        .get("x-opencode-directory-encoding")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if let Some(raw) = headers
        .get("x-opencode-directory")
        .and_then(|v| v.to_str().ok())
        && !raw.is_empty()
    {
        if header_encoding == "uri" {
            candidates
                .push(super::paths::decode_uri_component(raw).unwrap_or_else(|| raw.to_string()));
        } else {
            candidates.push(raw.to_string());
        }
    }
    if let Some(directory) = query.get("directory")
        && !directory.is_empty()
    {
        candidates.push(directory.clone());
    }

    if !candidates.is_empty() {
        let mut last_error = None;
        for candidate in &candidates {
            match validate_directory_path(candidate).await {
                Ok(validated) => {
                    return ProjectDirectory {
                        directory: Some(validated.0),
                        requested_directory: Some(validated.1),
                        error: None,
                    };
                }
                Err(error) => last_error = Some(error),
            }
        }
        return ProjectDirectory {
            directory: None,
            requested_directory: None,
            error: last_error,
        };
    }

    let settings = read_settings_document();
    if let Some(last_directory) = settings
        .get("lastDirectory")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        && let Ok(validated) = validate_directory_path(last_directory).await
    {
        return ProjectDirectory {
            directory: Some(validated.0),
            requested_directory: Some(validated.1),
            error: None,
        };
    }

    let active_id = settings
        .get("activeProjectId")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let projects = sanitize_project_paths(&settings);
    if projects.is_empty() {
        return ProjectDirectory {
            directory: None,
            requested_directory: None,
            error: Some("Directory parameter or active project is required".to_string()),
        };
    }
    let active = projects
        .iter()
        .find(|(id, _)| !id.is_empty() && *id == active_id)
        .or_else(|| projects.first());
    let Some((_, active_path)) = active.cloned() else {
        return ProjectDirectory {
            directory: None,
            requested_directory: None,
            error: Some("Directory parameter or active project is required".to_string()),
        };
    };
    match validate_directory_path(&active_path).await {
        Ok(validated) => ProjectDirectory {
            directory: Some(validated.0),
            requested_directory: Some(validated.1),
            error: None,
        },
        Err(error) => ProjectDirectory {
            directory: None,
            requested_directory: None,
            error: Some(error),
        },
    }
}

/// project-directory-runtime.js `validateDirectoryPath` — (canonical
/// directory, lexical requested directory).
///
/// 中文说明：校验候选目录并返回 (canonical 目录, 词法目录) 二元组；
/// 目录必须存在、可访问且确实是目录。错误文案与 JS 逐字对齐。
async fn validate_directory_path(candidate: &str) -> Result<(PathBuf, PathBuf), String> {
    let trimmed = candidate.trim();
    if trimmed.is_empty() {
        return Err("Directory parameter is required".to_string());
    }
    let resolved = resolve_path(&normalize_directory_path(trimmed));
    let metadata = match tokio::fs::metadata(&resolved).await {
        Ok(metadata) => metadata,
        Err(error) => {
            return Err(match error.kind() {
                std::io::ErrorKind::NotFound => "Directory not found".to_string(),
                std::io::ErrorKind::PermissionDenied => "Access to directory denied".to_string(),
                _ => "Failed to validate directory".to_string(),
            });
        }
    };
    if !metadata.is_dir() {
        return Err("Specified path is not a directory".to_string());
    }
    match realpath(&resolved) {
        Ok(canonical) => Ok((canonical, resolved)),
        Err(_) => Err("Failed to validate directory".to_string()),
    }
}

/// 读取 settings.json 文档；根目录优先取 OMPCHAMBER_DATA_DIR，回落
/// `~/.config/ompchamber`。任何读取/解析失败都得到 `Value::Null`。
fn read_settings_document() -> serde_json::Value {
    // The JS chain reads settings through the settings runtime (honors
    // OMPCHAMBER_DATA_DIR); only workspace *admission* uses the hardcoded
    // ~/.config/ompchamber root.
    let root = std::env::var("OMPCHAMBER_DATA_DIR")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(user_config_root);
    let path = root.join("settings.json");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or(serde_json::Value::Null)
}

/// Minimal `sanitizeProjects`: keep entries that carry a non-empty `path`.
/// (The full JS sanitizer also realpaths each project; that is a persistence
/// concern owned by the settings module port.)
///
/// 中文说明：最小化的项目列表清洗——只保留 `path` 非空的条目并输出
/// (id, path)；完整的 realpath 清洗属于 settings 模块的持久化职责。
fn sanitize_project_paths(settings: &serde_json::Value) -> Vec<(String, String)> {
    settings
        .get("projects")
        .and_then(|v| v.as_array())
        .map(|projects| {
            projects
                .iter()
                .filter_map(|project| {
                    let path = project
                        .get("path")
                        .and_then(|v| v.as_str())?
                        .trim()
                        .to_string();
                    if path.is_empty() {
                        return None;
                    }
                    let id = project
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    Some((id, path))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 工作区准入与路径规范化的单元测试。
#[cfg(test)]
mod tests {
    use super::*;

    /// 由键值对数组构造 query map 的测试辅助。
    fn query(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// 验证准入白名单：项目根与用户配置根放行，其它路径与空白路径拒绝。
    #[test]
    fn workspace_path_admits_project_and_config_roots_only() {
        assert!(resolve_workspace_path("/repo/file.txt", Some(Path::new("/repo"))).is_ok());
        assert_eq!(
            resolve_workspace_path("/etc/passwd", Some(Path::new("/repo"))).unwrap_err(),
            "Path is outside of active workspace"
        );
        assert_eq!(
            resolve_workspace_path("   ", Some(Path::new("/repo"))).unwrap_err(),
            "Path is required"
        );
        // The user config root is always admitted.
        let config_child = user_config_root().join("themes").join("x.css");
        assert!(
            resolve_workspace_path(&config_child.to_string_lossy(), Some(Path::new("/repo")))
                .is_ok()
        );
    }

    /// 验证 `..` 穿越在词法层面即被捕获（resolve 先折叠再判定）。
    #[test]
    fn traversal_through_dots_is_caught_lexically() {
        // path.resolve collapses first: /repo/sub/../../etc/passwd -> /etc/passwd
        assert_eq!(
            resolve_workspace_path("/repo/sub/../../etc/passwd", Some(Path::new("/repo")))
                .unwrap_err(),
            "Path is outside of active workspace"
        );
    }
}
