//! Port of `server/lib/walkthrough/schema.js`.
//!
//! Shape of the walkthrough the model must produce, plus normalization of what
//! it actually produced. The model is only ever trusted for prose and grouping
//! — every anchor it returns is re-resolved against the digest here, and
//! anything that does not resolve is dropped rather than rendered as a broken
//! stop.
//!
//! 中文说明：`server/lib/walkthrough/schema.js` 的移植。本模块定义模型必须
//! 产出的 walkthrough 形状，并对其实际产出做归一化。模型只被信任提供文字
//! 与分组：它返回的每个锚点都会在此对照 digest 重新解析，解析不到的直接
//! 丢弃，而不是渲染成一个坏掉的 stop。

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

/// walkthrough 缓存条目的格式版本；版本不匹配的旧条目按过期处理。
pub const WALKTHROUGH_VERSION: i64 = 1;

// Bumping this invalidates every cached walkthrough, which is the point: a
// changed prompt produces different output and old entries would misrepresent
// what the current code would say.
/// prompt 版本：prompt 一变就提升此值，让全部缓存 walkthrough 失效。
pub const PROMPT_VERSION: i64 = 3;

/// 单个 walkthrough 允许的章节数上限。
pub const MAX_CHAPTERS: usize = 6;
/// 所有章节合计的 stop 数上限。
pub const MAX_STOPS: usize = 16;
/// 单个 stop 可锚定的 hunk 数上限。
pub const MAX_HUNKS_PER_STOP: usize = 14;
/// 章节标题的字符数上限（超出即截断）。
pub const MAX_CHAPTER_TITLE_CHARS: usize = 24;

/// 章节允许的图标枚举；模型给出枚举外的值时回退 `doc`。
pub const CHAPTER_ICONS: [&str; 6] = ["bug", "wrench", "path", "flask", "doc", "gear"];
/// stop 允许的重要性等级；非法值回退 `normal`。
pub const STOP_IMPORTANCE: [&str; 3] = ["critical", "normal", "context"];

/// JS `\s` 视为空白的字符集合，用于逐字复刻 JS 的 trim 行为。
const JS_WHITESPACE: [char; 6] = [' ', '\t', '\n', '\r', '\x0b', '\x0c'];

/// `responseSchema`: the JSON Schema handed to the model layer. Field order
/// mirrors the JS literal; consumers (the small-model seam) treat it as data.
/// 中文补充：该 schema 会作为 structured-output 的 `response_schema` 随请求
/// 发给 provider；拒绝它的 provider 触发 prompt 内嵌形状说明的降级路径。
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
/// 中文补充：所有模型产出的字符串字段都经此归一化，杜绝前后空白与超长标题。
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
/// 中文补充：章节是 stop 的分组；空章节（无 stop）在归一化时被整体丢弃。
#[derive(Debug, Clone, PartialEq)]
pub struct Chapter {
/// 本地稳定 id，形如 `chapter-1`，供前端与缓存引用。
    pub id: String,
/// 章节标题；为空时回退「Part N」，超长被截断。
    pub title: String,
/// 章节图标，限定枚举值，非法回退 `doc`。
    pub icon: String,
/// 章节一句话简介。
    pub blurb: String,
/// 章节内的 stop 列表。
    pub stops: Vec<Stop>,
}

/// A normalized walkthrough stop, anchored to real hunk ids.
/// 中文补充：stop 是讲解的最小单元，`hunk_ids` 只含解析成功的真实锚点。
#[derive(Debug, Clone, PartialEq)]
pub struct Stop {
/// 本地稳定 id，形如 `stop-1-1`（章序-步序）。
    pub id: String,
/// stop 标题；为空时回退「Step N」。
    pub title: String,
/// 锚定的真实 hunk id 列表；已按 digest 解析别名并全局去重。
    pub hunk_ids: Vec<String>,
/// 重要性等级，限定枚举值，非法回退 `normal`。
    pub importance: String,
/// 讲解正文；trim 后为空的 stop 被整体丢弃。
    pub prose: String,
}

/// `normalizeWalkthrough`'s output.
/// 中文补充：归一化结果既写入缓存也直接序列化给客户端。
#[derive(Debug, Clone, PartialEq)]
pub struct NormalizedWalkthrough {
/// walkthrough 标题；为空时回退「Change walkthrough」。
    pub title: String,
/// 本次变更的侧重点描述。
    pub focus: String,
/// 归一化后的章节列表，至少一章否则报错。
    pub chapters: Vec<Chapter>,
/// 因别名解析失败而被丢弃的锚点计数，随结果透出。
    pub dropped_anchors: usize,
}

/// 归一化结果的 wire 序列化。
impl NormalizedWalkthrough {
    /// The wire shape the client and the cache store
    /// (`{title, focus, chapters, droppedAnchors}`).
/// 中文补充：键为 camelCase，与 JS 端约定一致。
    pub fn to_value(&self) -> Value {
        json!({
            "title": self.title,
            "focus": self.focus,
            "chapters": self.chapters.iter().map(Chapter::to_value).collect::<Vec<_>>(),
            "droppedAnchors": self.dropped_anchors,
        })
    }
}

/// 章节 wire 序列化。
impl Chapter {
/// 序列化为 wire 形状中的章节点。
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

/// stop wire 序列化。
impl Stop {
/// 序列化为 wire 形状中的 stop 节点。
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
/// 中文补充：解析不到的锚点计入 `dropped_anchors` 并跳过；同一 hunk 只归
/// 第一个引用它的 stop；全部 stop 都落空时报错（消息与 JS 一致）。
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
/// 中文补充：依次尝试「整体解析 → 剥围栏后解析 → 首个 { 到最右 } 的收缩
/// 扫描」，模拟 JS 端对不守规矩输出的容忍度。
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

/// 剥掉开头的一个代码围栏前缀（可带大小写不敏感的 json 标记）及其后空白。
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

/// 剥掉结尾空白之后的一个代码围栏后缀。
fn strip_trailing_fence(text: &str) -> &str {
    let without_trailing_space = text.trim_end_matches(JS_WHITESPACE);
    without_trailing_space
        .strip_suffix("```")
        .unwrap_or(without_trailing_space)
}

/// schema 模块单测：别名解析与裁剪规则、wire 序列化、模型 JSON 的容错提取。
#[cfg(test)]
mod tests {
    use super::*;

/// 构造测试用别名映射：h1/h2/h3 → 真实 hunk id。
    fn aliases() -> HashMap<String, String> {
        HashMap::from([
            ("h1".to_string(), "working:src/a.ts:aaaa1111".to_string()),
            ("h2".to_string(), "working:src/a.ts:bbbb2222".to_string()),
            ("h3".to_string(), "working:src/b.ts:cccc3333".to_string()),
        ])
    }

/// 以给定章节构造一个完整的原始 walkthrough 响应对象。
    fn walkthrough(chapters: Value) -> Value {
        json!({ "title": "Change", "focus": "why", "chapters": chapters })
    }

/// 构造单章节 JSON。
    fn chapter(stops: Value) -> Value {
        json!({ "title": "Data", "icon": "doc", "blurb": "", "stops": stops })
    }

/// 构造单个 stop JSON。
    fn stop(title: &str, hunks: Value, importance: &str, prose: &str) -> Value {
        json!({ "title": title, "hunks": hunks, "importance": importance, "prose": prose })
    }

    /// 别名被映射为真实 hunk id，并分配稳定的本地章节/stop id。
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

    /// 模型编造的别名被丢弃并计数，而不是渲染成坏锚点。
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

    /// 同一 hunk 只锚定到第一个引用它的 stop。
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

    /// 失去全部锚点或正文为空的 stop 被整体丢弃。
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

    /// 所有 stop 都落空时归一化报「no usable stops」错误。
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

    /// 非对象响应被拒绝；数组按 JS 语义走「no chapters」错误路径。
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

    /// 超长章节标题被截断，未知 icon/importance 回退默认值。
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

    /// stop 总数受上限约束（本例实际受别名池大小的限制）。
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

    /// 干净的 JSON 对象直接解析成功。
    #[test]
    fn parses_a_clean_object() {
        assert_eq!(
            parse_model_json("{\"title\":\"x\"}").unwrap(),
            json!({ "title": "x" })
        );
    }

    /// 包在代码围栏中的 JSON 能被剥出并解析。
    #[test]
    fn unwraps_a_fenced_block() {
        assert_eq!(
            parse_model_json("```json\n{\"title\":\"x\"}\n```").unwrap(),
            json!({ "title": "x" })
        );
    }

    /// JSON 对象之后夹杂的闲话被剥离，仍能恢复对象。
    #[test]
    fn recovers_an_object_followed_by_stray_prose() {
        assert_eq!(
            parse_model_json("{\"title\":\"x\"}\n\nHope that helps!").unwrap(),
            json!({ "title": "x" })
        );
    }

    /// 空响应、无对象、非法 JSON 各自报出可区分的错误消息。
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

    /// 序列化输出使用 camelCase 键并携带 droppedAnchors 计数。
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
