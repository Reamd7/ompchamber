//! Shared `{ kind, message, ... }` error payload for the skills-catalog port
//! (`server/lib/skills-catalog/*`). The JS module returns plain
//! `{ ok: false, error: { kind, message } }` objects (with optional `sshOnly`
//! and `conflicts` extras); these structs serialize to exactly those shapes
//! so route handlers can map `authRequired` → 401, `conflicts` → 409 and
//! `invalidSource` → 400 without reshaping.
//!
//! 中文说明：skills-catalog 移植共用的 `{ kind, message, ... }` 错误载荷。
//! JS 模块返回 `{ ok: false, error: { kind, message } }`（可选 `sshOnly`
//! 与 `conflicts` 扩展）；这些结构体序列化为完全相同的形状，路由层可
//! 直接把 `authRequired` → 401、`conflicts` → 409、`invalidSource` → 400。

use serde::{Deserialize, Serialize};

/// One entry of a `conflicts` error payload (install.js pushes
/// `{ skillName, scope, source }` per conflicting skill).
/// 每个冲突技能一条，随 conflicts 错误一起返回给前端二次确认。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictEntry {
    /// 冲突技能名。
    pub skill_name: String,
    /// 安装 scope（user/project）。
    pub scope: String,
    /// 目标目录树标签（opencode/agents）。
    pub source: String,
}

/// `{ kind, message, sshOnly?, conflicts? }` — the error half of every
/// skills-catalog result object.
/// 可选扩展字段仅在存在时序列化，与 JS 对象保持同形。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogError {
    /// 错误类别（invalidSource、gitUnavailable、authRequired、
    /// networkError、unknown、conflicts）。
    pub kind: String,
    /// 人类可读的错误消息（与 JS 文案逐字一致）。
    pub message: String,
    /// authRequired 时为 true，表示仅 SSH 可访问。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_only: Option<bool>,
    /// conflicts 错误的逐技能冲突列表。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflicts: Option<Vec<ConflictEntry>>,
}

/// CatalogError 的构造函数集：基础构造加各 kind 的语义化快捷方式。
impl CatalogError {
    /// 基础构造：指定 kind 与消息。
    pub fn new(kind: &str, message: impl Into<String>) -> Self {
        CatalogError {
            kind: kind.to_string(),
            message: message.into(),
            ssh_only: None,
            conflicts: None,
        }
    }

    /// `kind: "invalidSource"` — malformed repository source / params.
    /// 参数或源字符串不合法（路由层映射 400）。
    pub fn invalid_source(message: impl Into<String>) -> Self {
        Self::new("invalidSource", message)
    }

    /// `kind: "gitUnavailable"` — `assertGitAvailable` failure.
    /// git 不可用（`git --version` 探测失败）。
    pub fn git_unavailable() -> Self {
        Self::new("gitUnavailable", "Git is not available in PATH")
    }

    /// `kind: "authRequired"` with `sshOnly: true` (clone.js auth mapping).
    /// 克隆认证失败且仅 SSH key 可解（映射 401）。
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
    /// 克隆失败输出（或兜底文本）归为网络错误。
    pub fn network(message: impl Into<String>) -> Self {
        Self::new("networkError", message)
    }

    /// `kind: "unknown"` — catch-all.
    /// 兜底 kind。
    pub fn unknown(message: impl Into<String>) -> Self {
        Self::new("unknown", message)
    }

    /// `kind: "conflicts"` with the per-skill list (install.js).
    /// 目标已存在同名技能（映射 409），携带逐技能冲突列表。
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
