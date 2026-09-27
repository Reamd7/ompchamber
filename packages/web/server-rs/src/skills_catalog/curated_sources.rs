//! Port of `server/lib/skills-catalog/curated-sources.js`.
//!
//! 中文说明：预置（curated）技能源清单及其读取入口，内容对齐
//! `server/lib/skills-catalog/curated-sources.js`。

use serde::Serialize;

/// A curated skill source entry. `defaultSubpath` is absent for sources that
/// scan the repository root (`mattpocock/skills`).
/// 序列化为 camelCase；`defaultSubpath` 仅在存在时输出。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CuratedSource {
    /// 稳定标识（如 `anthropic`），供前端引用。
    pub id: &'static str,
    /// 展示用的来源名称。
    pub label: &'static str,
    /// 给用户看的来源描述。
    pub description: &'static str,
    /// 仓库源（`owner/repo` 简写）。
    pub source: &'static str,
    /// 默认扫描的子目录；None 表示扫仓库根（如 `mattpocock/skills`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_subpath: Option<&'static str>,
    /// 来源类型（当前均为 `github`）。
    pub source_type: &'static str,
}

/// 预置来源静态清单：Anthropic、OpenAI、Cursor 与 Matt Pocock。
const CURATED_SKILLS_SOURCES: &[CuratedSource] = &[
    CuratedSource {
        id: "anthropic",
        label: "Anthropic",
        description: "Anthropic's public skills repository",
        source: "anthropics/skills",
        default_subpath: Some("skills"),
        source_type: "github",
    },
    CuratedSource {
        id: "openai",
        label: "OpenAI",
        description: "OpenAI's curated skills",
        source: "openai/skills",
        default_subpath: Some("skills/.curated"),
        source_type: "github",
    },
    CuratedSource {
        id: "cursor",
        label: "Cursor",
        description: "Cursor's plugin skills",
        source: "cursor/plugins",
        default_subpath: Some("pstack/skills"),
        source_type: "github",
    },
    CuratedSource {
        id: "mattpocock",
        label: "Matt Pocock",
        description: "Matt Pocock skills collection",
        source: "mattpocock/skills",
        default_subpath: None,
        source_type: "github",
    },
];

/// `getCuratedSkillsSources()`: a fresh copy of the curated list
/// (JS returns `CURATED_SKILLS_SOURCES.slice()`).
/// 返回清单的全新拷贝，避免调用方改动共享静态数据。
pub fn get_curated_skills_sources() -> Vec<CuratedSource> {
    CURATED_SKILLS_SOURCES.to_vec()
}

/// curated-sources 测试：清单内容与序列化形状。
#[cfg(test)]
mod tests {
    use super::*;

    /// 行为契约：清单包含 Anthropic 条目且字段完整。
    #[test]
    fn includes_the_anthropic_curated_source() {
        let anthropic = get_curated_skills_sources()
            .into_iter()
            .find(|source| source.id == "anthropic")
            .expect("anthropic source present");
        assert_eq!(anthropic.label, "Anthropic");
        assert_eq!(anthropic.source, "anthropics/skills");
        assert_eq!(anthropic.default_subpath, Some("skills"));
        assert_eq!(anthropic.source_type, "github");
    }

    /// 行为契约：序列化为 JS 侧形状，未设置的 defaultSubpath 被省略。
    #[test]
    fn serializes_to_the_js_shape() {
        let sources = get_curated_skills_sources();
        assert_eq!(sources.len(), 4);

        let value = serde_json::to_value(&sources).expect("serializes");
        let mattpocock = value
            .as_array()
            .expect("array")
            .iter()
            .find(|entry| entry.get("id").and_then(|v| v.as_str()) == Some("mattpocock"))
            .expect("mattpocock present");
        assert!(
            mattpocock.get("defaultSubpath").is_none(),
            "defaultSubpath omitted when unset: {mattpocock}"
        );
        assert_eq!(
            mattpocock.get("label").and_then(|v| v.as_str()),
            Some("Matt Pocock")
        );
    }
}
