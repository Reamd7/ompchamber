//! Port of `server/lib/projects/project-id.js`.
//!
//! Derives a stable, filesystem-safe project id from a directory path:
//! `path_<base64url(normalized path)>`. Shared by settings project
//! registration, agent-memory scoping, and opencode route handlers.
//!
//! 中文说明：从目录路径派生稳定且文件系统安全的项目 id——
//! path_ 前缀 + 归一化路径的 base64url。设置里的项目注册、agent-memory
//! 作用域与 opencode 路由都复用该派生规则。

use base64::Engine as _;

/// Backslash → forward slash, strip trailing slashes; a value that normalizes
/// to nothing falls back to the original (JS `|| value`).
/// 中文：反斜杠转正斜杠并去掉尾部斜杠；归一化后为空的值回退为原始
/// 输入（对齐 JS 的 || value），保证 "/" 不被吞掉。
fn normalize_project_path_for_id(value: &str) -> String {
    let replaced = value.replace('\\', "/");
    let stripped = replaced.trim_end_matches('/');
    if stripped.is_empty() {
        value.to_string()
    } else {
        stripped.to_string()
    }
}

/// URL-safe base64 without padding (Node `Buffer.toString('base64url')`).
/// 中文：无填充的 URL-safe base64，等价 Node 的 Buffer.toString('base64url')。
fn base64_url_no_pad(input: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(input)
}

/// `createProjectIdFromPath`: empty input (or whitespace-only after
/// normalization) yields `""`; otherwise `path_<base64url>`.
/// 中文：归一化并 trim 后为空的输入返回空串；否则返回
/// path_<base64url(归一化路径)>。
pub fn create_project_id_from_path(project_path: &str) -> String {
    let normalized = normalize_project_path_for_id(project_path);
    let trimmed = normalized.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    format!("path_{}", base64_url_no_pad(trimmed.as_bytes()))
}

/// create_project_id_from_path 的行为契约测试。
#[cfg(test)]
mod tests {
    use super::*;

    /// 验证：id 是 path_ 前缀 + 路径的 base64url 编码。
    #[test]
    fn derives_base64url_ids() {
        assert_eq!(create_project_id_from_path("abc"), "path_YWJj");
        assert_eq!(create_project_id_from_path("/abc"), "path_L2FiYw");
    }

    /// 验证：尾部斜杠与反斜杠归一后得到相同 id。
    #[test]
    fn normalizes_trailing_slashes_and_backslashes() {
        assert_eq!(
            create_project_id_from_path("/Users/x/proj/"),
            create_project_id_from_path("/Users/x/proj")
        );
        assert_eq!(
            create_project_id_from_path("\\Users\\x\\proj"),
            create_project_id_from_path("/Users/x/proj")
        );
        assert_eq!(create_project_id_from_path("/abc///"), "path_L2FiYw");
    }

    /// 验证：根路径 "/" 归一为空后回退原值，斜杠参与编码。
    #[test]
    fn a_root_only_path_keeps_its_slash() {
        // "/" normalizes to "" then falls back to the original value.
        assert_eq!(create_project_id_from_path("/"), "path_Lw");
    }

    /// 验证：空串与纯空白路径返回空 id。
    #[test]
    fn empty_and_whitespace_paths_yield_empty_id() {
        assert_eq!(create_project_id_from_path(""), "");
        assert_eq!(create_project_id_from_path("   "), "");
    }

    /// 验证：生成的 id 只含文件系统安全字符。
    #[test]
    fn ids_stay_filesystem_safe() {
        let id = create_project_id_from_path("/Users/x/projects/openchamber");
        assert!(id.starts_with("path_"));
        assert!(
            id.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'))
        );
    }
}
