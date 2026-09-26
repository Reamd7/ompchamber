//! Port of `server/lib/walkthrough/schema.js`.
//!
//! Shape of the walkthrough the model must produce, plus normalization of what
//! it actually produced. The model is only ever trusted for prose and grouping
//! — every anchor it returns is re-resolved against the digest here, and
//! anything that does not resolve is dropped rather than rendered as a broken
//! stop.

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

pub const WALKTHROUGH_VERSION: i64 = 1;

// Bumping this invalidates every cached walkthrough, which is the point: a
// changed prompt produces different output and old entries would misrepresent
// what the current code would say.
pub const PROMPT_VERSION: i64 = 3;

pub const MAX_CHAPTERS: usize = 6;
pub const MAX_STOPS: usize = 16;
pub const MAX_HUNKS_PER_STOP: usize = 14;
pub const MAX_CHAPTER_TITLE_CHARS: usize = 24;

pub const CHAPTER_ICONS: [&str; 6] = ["bug", "wrench", "path", "flask", "doc", "gear"];
pub const STOP_IMPORTANCE: [&str; 3] = ["critical", "normal", "context"];

const JS_WHITESPACE: [char; 6] = [' ', '\t', '\n', '\r', '\x0b', '\x0c'];

/// `responseSchema`: the JSON Schema handed to the model layer. Field order
/// mirrors the JS literal; consumers (the small-model seam) treat it as data.
pub fn response_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "title": { "type": "string" },
            "focus": { "type": "string" },
            "chapters": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "title": { "type": "string" },
                        "icon": { "type": "string", "enum": CHAPTER_ICONS },
                        "blurb": { "type": "string" },
                        "stops": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "title": { "type": "string" },
                                    "hunks": { "type": "array", "items": { "type": "string" } },
                                    "importance": { "type": "string", "enum": STOP_IMPORTANCE },
                                    "prose": { "type": "string" }
                                },
                                "required": ["title", "hunks", "importance", "prose"],
                                "additionalProperties": false
                            }
                        }
                    },
                    "required": ["title", "icon", "blurb", "stops"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["title", "focus", "chapters"],
        "additionalProperties": false
    })
}

/// `asString`: trimmed string or `""`, clamped to `max` characters when given.
fn as_string(value: Option<&Value>, max: Option<usize>) -> String {
    let Some(Value::String(raw)) = value else {
        return String::new();
    };
    let trimmed = raw.trim();
    match max {
        Some(max) if trimmed.chars().count() > max => trimmed.chars().take(max).collect(),
        _ => trimmed.to_string(),
    }
}

/// A normalized walkthrough chapter.
#[derive(Debug, Clone, PartialEq)]
pub struct Chapter {
    pub id: String,
    pub title: String,
    pub icon: String,
    pub blurb: String,
    pub stops: Vec<Stop>,
}

/// A normalized walkthrough stop, anchored to real hunk ids.
#[derive(Debug, Clone, PartialEq)]
pub struct Stop {
    pub id: String,
    pub title: String,
    pub hunk_ids: Vec<String>,
    pub importance: String,
    pub prose: String,
}

/// `normalizeWalkthrough`'s output.
#[derive(Debug, Clone, PartialEq)]
pub struct NormalizedWalkthrough {
    pub title: String,
    pub focus: String,
    pub chapters: Vec<Chapter>,
    pub dropped_anchors: usize,
}

impl NormalizedWalkthrough {
    /// The wire shape the client and the cache store
    /// (`{title, focus, chapters, droppedAnchors}`).
    pub fn to_value(&self) -> Value {
        json!({
            "title": self.title,
            "focus": self.focus,
            "chapters": self.chapters.iter().map(Chapter::to_value).collect::<Vec<_>>(),
            "droppedAnchors": self.dropped_anchors,
        })
    }
}

impl Chapter {
    fn to_value(&self) -> Value {
        json!({
            "id": self.id,
            "title": self.title,
            "icon": self.icon,
            "blurb": self.blurb,
            "stops": self.stops.iter().map(Stop::to_value).collect::<Vec<_>>(),
        })
    }
}

impl Stop {
    fn to_value(&self) -> Value {
        json!({
            "id": self.id,
            "title": self.title,
            "hunkIds": self.hunk_ids,
            "importance": self.importance,
            "prose": self.prose,
        })
    }
}

/// `normalizeWalkthrough`: turn a raw model response into a walkthrough
/// anchored to real hunk ids.
///
/// `id_by_alias` maps alias → real hunk id (from the digest). Errors carry the
/// JS message strings; callers map them to HTTP failures.
pub fn normalize_walkthrough(
    raw: &Value,
    id_by_alias: &HashMap<String, String>,
) -> Result<NormalizedWalkthrough, &'static str> {
    let raw_object = match raw {
        Value::Object(object) => object,
        // JS `typeof [] === 'object'`, so an array falls through to the
        // "no chapters" path rather than the "no object" path.
        Value::Array(_) => return Err("Model returned no usable stops for this diff"),
        _ => return Err("Model returned no walkthrough object"),
    };

    let raw_chapters = match raw_object.get("chapters") {
        Some(Value::Array(chapters)) => chapters.as_slice(),
        _ => &[],
    };

    let mut used_ids: HashSet<String> = HashSet::new();
    let mut dropped_anchors = 0usize;
    let mut stop_count = 0usize;

    let mut chapters: Vec<Chapter> = Vec::new();
    for (chapter_index, raw_chapter) in raw_chapters.iter().enumerate() {
        if chapters.len() >= MAX_CHAPTERS {
            break;
        }
        let Some(raw_chapter) = raw_chapter.as_object() else {
            continue;
        };

        let raw_stops = match raw_chapter.get("stops") {
            Some(Value::Array(stops)) => stops.as_slice(),
            _ => &[],
        };

        let mut stops: Vec<Stop> = Vec::new();
        for raw_stop in raw_stops {
            if stop_count >= MAX_STOPS {
                break;
            }
            let Some(raw_stop) = raw_stop.as_object() else {
                continue;
            };

            let raw_aliases = match raw_stop.get("hunks") {
                Some(Value::Array(aliases)) => aliases.as_slice(),
                _ => &[],
            };

            let mut hunk_ids: Vec<String> = Vec::new();
            for alias in raw_aliases {
                let key = match alias {
                    Value::String(alias) => alias.trim().to_string(),
                    _ => String::new(),
                };
                let Some(id) = id_by_alias.get(&key) else {
                    dropped_anchors += 1;
                    continue;
                };
                // One hunk belongs to exactly one stop; a model that anchors
                // the same code twice would otherwise render it twice in the
                // stream.
                if used_ids.contains(id) {
                    continue;
                }
                if hunk_ids.len() >= MAX_HUNKS_PER_STOP {
                    break;
                }
                used_ids.insert(id.clone());
                hunk_ids.push(id.clone());
            }

            let prose = as_string(raw_stop.get("prose"), None);
            if hunk_ids.is_empty() || prose.is_empty() {
                continue;
            }

            stop_count += 1;
            let importance = raw_stop
                .get("importance")
                .and_then(Value::as_str)
                .filter(|value| STOP_IMPORTANCE.contains(value))
                .unwrap_or("normal");
            let title = as_string(raw_stop.get("title"), None);
            stops.push(Stop {
                id: format!("stop-{}-{}", chapter_index + 1, stops.len() + 1),
                title: if title.is_empty() {
                    format!("Step {stop_count}")
                } else {
                    title
                },
                hunk_ids,
                importance: importance.to_string(),
                prose,
            });
        }

        if stops.is_empty() {
            continue;
        }

        let icon = raw_chapter
            .get("icon")
            .and_then(Value::as_str)
            .filter(|value| CHAPTER_ICONS.contains(value))
            .unwrap_or("doc");
        let title = as_string(raw_chapter.get("title"), Some(MAX_CHAPTER_TITLE_CHARS));
        chapters.push(Chapter {
            id: format!("chapter-{}", chapters.len() + 1),
            title: if title.is_empty() {
                format!("Part {}", chapters.len() + 1)
            } else {
                title
            },
            icon: icon.to_string(),
            blurb: as_string(raw_chapter.get("blurb"), None),
            stops,
        });
    }

    if chapters.is_empty() {
        return Err("Model returned no usable stops for this diff");
    }

    let title = as_string(raw_object.get("title"), None);
    Ok(NormalizedWalkthrough {
        title: if title.is_empty() {
            "Change walkthrough".to_string()
        } else {
            title
        },
        focus: as_string(raw_object.get("focus"), None),
        chapters,
        dropped_anchors,
    })
}

/// `parseModelJson`: extract a JSON object from a model response that may or
/// may not honour the schema — some providers wrap it in prose or a fenced
/// block. Errors carry the JS message strings.
pub fn parse_model_json(text: &str) -> Result<Value, &'static str> {
    if text.trim().is_empty() {
        return Err("Model returned an empty response");
    }

    let trimmed = text.trim();
    // `.replace(/^```(?:json)?\s*/i, '')` — one optional leading fence with
    // an optional case-insensitive `json` word.
    let without_fence = strip_leading_fence(trimmed);
    // `.replace(/\s*```$/, '')` — one trailing fence after trailing space.
    let without_fence = strip_trailing_fence(without_fence);

    if let Ok(value) = serde_json::from_str(without_fence) {
        return Ok(value);
    }
    // Fall through to a bounded scan for the outermost object.

    let Some(start) = without_fence.find('{') else {
        return Err("Model response contained no JSON object");
    };

    let mut end = without_fence.rfind('}');
    while let Some(at) = end.filter(|&at| at > start) {
        if let Ok(value) = serde_json::from_str(&without_fence[start..at + 1]) {
            return Ok(value);
        }
        // Keep shrinking from the right: `lastIndexOf('}', end - 1)`.
        end = without_fence[..at].rfind('}');
    }

    Err("Model response was not valid JSON")
}

fn strip_leading_fence(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("```") else {
        return text;
    };
    // `(?:json)?` case-insensitive, then `\s*`.
    let bytes = rest.as_bytes();
    if bytes.len() >= 4 && bytes[..4].eq_ignore_ascii_case(b"json") {
        rest[4..].trim_start_matches(JS_WHITESPACE)
    } else {
        rest.trim_start_matches(JS_WHITESPACE)
    }
}

fn strip_trailing_fence(text: &str) -> &str {
    let without_trailing_space = text.trim_end_matches(JS_WHITESPACE);
    without_trailing_space
        .strip_suffix("```")
        .unwrap_or(without_trailing_space)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aliases() -> HashMap<String, String> {
        HashMap::from([
            ("h1".to_string(), "working:src/a.ts:aaaa1111".to_string()),
            ("h2".to_string(), "working:src/a.ts:bbbb2222".to_string()),
            ("h3".to_string(), "working:src/b.ts:cccc3333".to_string()),
        ])
    }

    fn walkthrough(chapters: Value) -> Value {
        json!({ "title": "Change", "focus": "why", "chapters": chapters })
    }

    fn chapter(stops: Value) -> Value {
        json!({ "title": "Data", "icon": "doc", "blurb": "", "stops": stops })
    }

    fn stop(title: &str, hunks: Value, importance: &str, prose: &str) -> Value {
        json!({ "title": title, "hunks": hunks, "importance": importance, "prose": prose })
    }

    #[test]
    fn maps_aliases_to_real_hunk_ids_and_assigns_stable_local_ids() {
        let result = normalize_walkthrough(
            &walkthrough(json!([chapter(json!([stop(
                "New field",
                json!(["h1", "h2"]),
                "critical",
                "Adds a field."
            )]))])),
            &aliases(),
        )
        .expect("normalizes");

        assert_eq!(result.chapters[0].id, "chapter-1");
        assert_eq!(result.chapters[0].stops[0].id, "stop-1-1");
        assert_eq!(
            result.chapters[0].stops[0].hunk_ids,
            vec!["working:src/a.ts:aaaa1111", "working:src/a.ts:bbbb2222"]
        );
        assert_eq!(result.chapters[0].stops[0].importance, "critical");
    }

    #[test]
    fn drops_invented_aliases_instead_of_rendering_a_broken_anchor() {
        let result = normalize_walkthrough(
            &walkthrough(json!([chapter(json!([stop(
                "Mixed",
                json!(["h1", "h99", "nonsense"]),
                "normal",
                "Something."
            )]))])),
            &aliases(),
        )
        .expect("normalizes");

        assert_eq!(
            result.chapters[0].stops[0].hunk_ids,
            vec!["working:src/a.ts:aaaa1111"]
        );
        assert_eq!(result.dropped_anchors, 2);
    }

    #[test]
    fn anchors_each_hunk_to_a_single_stop() {
        let result = normalize_walkthrough(
            &walkthrough(json!([chapter(json!([
                stop("First", json!(["h1"]), "normal", "One."),
                stop("Second", json!(["h1", "h2"]), "normal", "Two.")
            ]))])),
            &aliases(),
        )
        .expect("normalizes");

        assert_eq!(
            result.chapters[0].stops[0].hunk_ids,
            vec!["working:src/a.ts:aaaa1111"]
        );
        assert_eq!(
            result.chapters[0].stops[1].hunk_ids,
            vec!["working:src/a.ts:bbbb2222"]
        );
    }

    #[test]
    fn discards_stops_left_with_no_anchor_or_no_prose() {
        let result = normalize_walkthrough(
            &walkthrough(json!([chapter(json!([
                stop("Ghost", json!(["h99"]), "normal", "About nothing."),
                stop("Silent", json!(["h1"]), "normal", "   "),
                stop("Real", json!(["h2"]), "normal", "Actual explanation.")
            ]))])),
            &aliases(),
        )
        .expect("normalizes");

        let titles: Vec<_> = result.chapters[0]
            .stops
            .iter()
            .map(|stop| stop.title.as_str())
            .collect();
        assert_eq!(titles, vec!["Real"]);
    }

    #[test]
    fn rejects_a_response_whose_stops_all_fall_away() {
        let error = normalize_walkthrough(
            &walkthrough(json!([chapter(json!([stop(
                "Ghost",
                json!(["h99"]),
                "normal",
                "x"
            )]))])),
            &aliases(),
        )
        .unwrap_err();

        assert_eq!(error, "Model returned no usable stops for this diff");
    }

    #[test]
    fn rejects_a_non_object_response() {
        assert_eq!(
            normalize_walkthrough(&Value::Null, &aliases()).unwrap_err(),
            "Model returned no walkthrough object"
        );
        // JS treats arrays as objects, so they reach the "no chapters" path.
        assert_eq!(
            normalize_walkthrough(&json!([]), &aliases()).unwrap_err(),
            "Model returned no usable stops for this diff"
        );
    }

    #[test]
    fn clamps_chapter_titles_and_falls_back_on_unknown_enums() {
        let result = normalize_walkthrough(
            &walkthrough(json!([{
                "title": "An extremely long chapter title that will not fit the column",
                "icon": "rocket",
                "blurb": "",
                "stops": [stop("A", json!(["h1"]), "urgent", "Text.")]
            }])),
            &aliases(),
        )
        .expect("normalizes");

        assert!(result.chapters[0].title.chars().count() <= 24);
        assert_eq!(result.chapters[0].icon, "doc");
        assert_eq!(result.chapters[0].stops[0].importance, "normal");
    }

    #[test]
    fn caps_the_total_number_of_stops() {
        let many: Vec<Value> = (0..30)
            .map(|index| {
                let alias = ["h1", "h2", "h3"][index % 3];
                stop(&format!("Stop {index}"), json!([alias]), "normal", "Text.")
            })
            .collect();

        let result = normalize_walkthrough(&walkthrough(json!([chapter(json!(many))])), &aliases())
            .expect("normalizes");

        let total: usize = result.chapters.iter().map(|c| c.stops.len()).sum();
        assert!(total <= MAX_STOPS);
        // Only three aliases exist and each is used once, so the real cap here
        // is the alias pool, not the stop limit.
        assert_eq!(total, 3);
    }

    #[test]
    fn parses_a_clean_object() {
        assert_eq!(
            parse_model_json("{\"title\":\"x\"}").unwrap(),
            json!({ "title": "x" })
        );
    }

    #[test]
    fn unwraps_a_fenced_block() {
        assert_eq!(
            parse_model_json("```json\n{\"title\":\"x\"}\n```").unwrap(),
            json!({ "title": "x" })
        );
    }

    #[test]
    fn recovers_an_object_followed_by_stray_prose() {
        assert_eq!(
            parse_model_json("{\"title\":\"x\"}\n\nHope that helps!").unwrap(),
            json!({ "title": "x" })
        );
    }

    #[test]
    fn fails_loudly_on_unusable_output() {
        assert_eq!(
            parse_model_json("").unwrap_err(),
            "Model returned an empty response"
        );
        assert_eq!(
            parse_model_json("no json at all").unwrap_err(),
            "Model response contained no JSON object"
        );
        assert_eq!(
            parse_model_json("{\"broken\":").unwrap_err(),
            "Model response was not valid JSON"
        );
    }

    #[test]
    fn serializes_the_wire_shape_with_camel_case_keys() {
        let result = normalize_walkthrough(
            &walkthrough(json!([chapter(json!([stop(
                "Real",
                json!(["h1"]),
                "normal",
                "Text."
            )]))])),
            &aliases(),
        )
        .expect("normalizes");

        let value = result.to_value();
        assert_eq!(
            value["chapters"][0]["stops"][0]["hunkIds"],
            json!(["working:src/a.ts:aaaa1111"])
        );
        assert_eq!(value["droppedAnchors"], json!(0));
    }
}
