//! Port of `server/lib/skills-catalog/curated-sources.js`.

use serde::Serialize;

/// A curated skill source entry. `defaultSubpath` is absent for sources that
/// scan the repository root (`mattpocock/skills`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CuratedSource {
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub source: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_subpath: Option<&'static str>,
    pub source_type: &'static str,
}

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
pub fn get_curated_skills_sources() -> Vec<CuratedSource> {
    CURATED_SKILLS_SOURCES.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

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
