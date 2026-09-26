//! Shared `{ kind, message, ... }` error payload for the skills-catalog port
//! (`server/lib/skills-catalog/*`). The JS module returns plain
//! `{ ok: false, error: { kind, message } }` objects (with optional `sshOnly`
//! and `conflicts` extras); these structs serialize to exactly those shapes
//! so route handlers can map `authRequired` → 401, `conflicts` → 409 and
//! `invalidSource` → 400 without reshaping.

use serde::{Deserialize, Serialize};

/// One entry of a `conflicts` error payload (install.js pushes
/// `{ skillName, scope, source }` per conflicting skill).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictEntry {
    pub skill_name: String,
    pub scope: String,
    pub source: String,
}

/// `{ kind, message, sshOnly?, conflicts? }` — the error half of every
/// skills-catalog result object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogError {
    pub kind: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_only: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflicts: Option<Vec<ConflictEntry>>,
}

impl CatalogError {
    pub fn new(kind: &str, message: impl Into<String>) -> Self {
        CatalogError {
            kind: kind.to_string(),
            message: message.into(),
            ssh_only: None,
            conflicts: None,
        }
    }

    /// `kind: "invalidSource"` — malformed repository source / params.
    pub fn invalid_source(message: impl Into<String>) -> Self {
        Self::new("invalidSource", message)
    }

    /// `kind: "gitUnavailable"` — `assertGitAvailable` failure.
    pub fn git_unavailable() -> Self {
        Self::new("gitUnavailable", "Git is not available in PATH")
    }

    /// `kind: "authRequired"` with `sshOnly: true` (clone.js auth mapping).
    pub fn auth_required_ssh() -> Self {
        CatalogError {
            ssh_only: Some(true),
            ..Self::new(
                "authRequired",
                "Authentication required to access this repository",
            )
        }
    }

    /// `kind: "networkError"` — clone failure output (or the fallback text).
    pub fn network(message: impl Into<String>) -> Self {
        Self::new("networkError", message)
    }

    /// `kind: "unknown"` — catch-all.
    pub fn unknown(message: impl Into<String>) -> Self {
        Self::new("unknown", message)
    }

    /// `kind: "conflicts"` with the per-skill list (install.js).
    pub fn conflicts(conflicts: Vec<ConflictEntry>) -> Self {
        CatalogError {
            conflicts: Some(conflicts),
            ..Self::new(
                "conflicts",
                "Some skills already exist in the selected scope",
            )
        }
    }
}
