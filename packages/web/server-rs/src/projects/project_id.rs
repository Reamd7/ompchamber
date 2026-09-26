//! Port of `server/lib/projects/project-id.js`.
//!
//! Derives a stable, filesystem-safe project id from a directory path:
//! `path_<base64url(normalized path)>`. Shared by settings project
//! registration, agent-memory scoping, and opencode route handlers.

use base64::Engine as _;

/// Backslash → forward slash, strip trailing slashes; a value that normalizes
/// to nothing falls back to the original (JS `|| value`).
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
fn base64_url_no_pad(input: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(input)
}

/// `createProjectIdFromPath`: empty input (or whitespace-only after
/// normalization) yields `""`; otherwise `path_<base64url>`.
pub fn create_project_id_from_path(project_path: &str) -> String {
    let normalized = normalize_project_path_for_id(project_path);
    let trimmed = normalized.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    format!("path_{}", base64_url_no_pad(trimmed.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_base64url_ids() {
        assert_eq!(create_project_id_from_path("abc"), "path_YWJj");
        assert_eq!(create_project_id_from_path("/abc"), "path_L2FiYw");
    }

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

    #[test]
    fn a_root_only_path_keeps_its_slash() {
        // "/" normalizes to "" then falls back to the original value.
        assert_eq!(create_project_id_from_path("/"), "path_Lw");
    }

    #[test]
    fn empty_and_whitespace_paths_yield_empty_id() {
        assert_eq!(create_project_id_from_path(""), "");
        assert_eq!(create_project_id_from_path("   "), "");
    }

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
