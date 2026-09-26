//! Port of `server/lib/walkthrough/prompt.js`.

use serde_json::Value;

use super::languages::{DEFAULT_LANGUAGE, language_name};
use super::schema::{MAX_CHAPTER_TITLE_CHARS, MAX_CHAPTERS, MAX_HUNKS_PER_STOP, MAX_STOPS};

pub const SYSTEM: &str = "You are writing a guided review of a code change for the engineer who is about to read it.

Your job is to impose a reading order the diff itself does not have. A diff is ordered by file path, which is almost never the order in which the change makes sense. Group related hunks — across files — into stops, and order the stops so that each one is understandable given the ones before it.

What a good stop says:
- what this code now does differently, in terms of behavior, not syntax
- why the surrounding hunks belong together
- what a reviewer should check or be suspicious about, when there is something

What a bad stop says:
- \"Renamed X to Y\", \"Added a parameter\", \"Updated the imports\" — restating the diff in prose is worthless; the reader can already see it
- speculation about intent you cannot support from the code

Rules:
- Anchor every stop to hunk aliases from the digest, exactly as given (h1, h2, …). Never invent an alias.
- Anchor each hunk at most once, in the stop where it matters most.
- You do not have to cover every hunk. Mechanical changes are better left out than padded into a stop; whatever you omit is still shown to the reader separately.
- Order stops so the reader builds understanding: entry points and data shape before the code that consumes them.
- importance: \"critical\" for changes that carry real risk or drive the rest, \"context\" for supporting changes, \"normal\" otherwise.
- Stop titles name the thing the stop is about, not the act of reviewing it. \"Hardware keyboard bridge\" and \"Overflow menu removed\" tell a reader scanning the contents what they will find; \"Exercise the boundaries\" and \"Describe the contract\" do not.
- Write prose as plain sentences. No markdown, no bullet lists, no code fences.

Respond with a single JSON object and nothing else. (Some providers refuse a structured-output request unless the word \"json\" appears in the request, which is why this is stated explicitly.)";

// A reader who cannot follow English prose gets nothing out of a walkthrough,
// so the output language is the reader's, not the codebase's.
//
// Only prose is translated. Hunk aliases are keys the server resolves back to
// hunk ids, and `icon`/`importance` are enums the normalizer validates against
// fixed English values — translating either produces a walkthrough that drops
// its anchors or loses its styling, silently and completely. Identifiers taken
// from the diff stay verbatim for the same reason a translated function name
// would be unsearchable.
fn language_instruction(language: &str) -> String {
    let name = language_name(language);
    format!(
        "\nWrite all prose in {name}: the walkthrough title, the focus line, chapter titles and blurbs, and stop titles and prose. The reader of this review reads {name}.\n\nKeep these in English exactly as given, regardless of the prose language: hunk aliases (h1, h2, …), the \"icon\" values, and the \"importance\" values. Keep identifiers, file paths, and API names as they appear in the code — never translate them."
    )
}

/// `sizing({ fileCount, hunkCount })`.
fn sizing(file_count: usize, hunk_count: usize) -> String {
    let target_stops = {
        let rounded = (hunk_count as f64 / 2.5).round() as i64;
        rounded.clamp(1, MAX_STOPS as i64).max(1)
    };
    let target_chapters = if hunk_count <= 4 {
        1
    } else {
        let value = (target_stops as f64 / 3.0).ceil() as i64;
        value.clamp(1, MAX_CHAPTERS as i64)
    };

    format!(
        "\nThis change has {file_count} file(s) and {hunk_count} reviewable hunk(s).\n\nAim for about {target_stops} stop(s) across about {target_chapters} chapter(s); never exceed {MAX_STOPS} stops, {MAX_CHAPTERS} chapters, or {MAX_HUNKS_PER_STOP} hunks in one stop. Fewer, denser stops beat many thin ones.\n\nChapter titles render in a narrow column: at most {MAX_CHAPTER_TITLE_CHARS} characters, one or two words."
    )
}

/// `previousWalkthroughSection(previous)`: the stored walkthrough as prose for
/// a forced regeneration, with anchors deliberately stripped (they belong to
/// code that has moved).
fn previous_walkthrough_section(previous: Option<&Value>) -> String {
    let Some(previous) = previous else {
        return String::new();
    };
    let Some(chapters) = previous.get("chapters").and_then(Value::as_array) else {
        return String::new();
    };
    if chapters.is_empty() {
        return String::new();
    }

    let outline = chapters
        .iter()
        .map(|chapter| {
            let stops = chapter
                .get("stops")
                .and_then(Value::as_array)
                .map(|stops| {
                    stops
                        .iter()
                        .map(|stop| {
                            format!(
                                "  - {}: {}",
                                stop.get("title").and_then(Value::as_str).unwrap_or(""),
                                stop.get("prose").and_then(Value::as_str).unwrap_or("")
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            let title = chapter.get("title").and_then(Value::as_str).unwrap_or("");
            let blurb = chapter.get("blurb").and_then(Value::as_str).unwrap_or("");
            let head = if !blurb.is_empty() {
                format!("- {title} — {blurb}")
            } else {
                format!("- {title}")
            };
            if stops.is_empty() {
                head
            } else {
                format!("{head}\n{stops}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    let previous_title = previous.get("title").and_then(Value::as_str).unwrap_or("");
    format!(
        "\nA previous walkthrough of an earlier state of this change is below. The code has moved on since it was written, so its anchors are gone — deliberately, so you re-anchor everything against the current digest.\n\nKeep the stops that are still accurate and phrased well, revise the ones whose code changed, drop the ones whose code no longer exists, and add stops for work that is new. Do not preserve its structure out of loyalty; preserve it only where it still fits.\n\nPrevious walkthrough — \"{previous_title}\":\n{outline}\n"
    )
}

/// Used only when a provider rejects a schema request: the shape has to
/// travel in the prompt instead of the request body.
pub const JSON_SHAPE_INSTRUCTION: &str = "\nReturn ONLY a JSON object, with no prose around it and no markdown fences, in exactly this shape:\n{\"title\": string, \"focus\": string, \"chapters\": [{\"title\": string, \"icon\": \"bug\"|\"wrench\"|\"path\"|\"flask\"|\"doc\"|\"gear\", \"blurb\": string, \"stops\": [{\"title\": string, \"hunks\": [string], \"importance\": \"critical\"|\"normal\"|\"context\", \"prose\": string}]}]}";

/// `buildPrompt`'s input.
pub struct PromptInput<'a> {
    /// Pre-serialized digest (`buildDigest(...).digest_json`).
    pub digest_json: &'a str,
    pub file_count: usize,
    pub hunk_count: usize,
    /// Normalized source (`{kind, ...}`).
    pub source: &'a Value,
    pub previous_walkthrough: Option<&'a Value>,
    pub language: Option<&'a str>,
}

/// `buildPrompt`: the user prompt plus the system prompt for a generation.
pub fn build_prompt(input: PromptInput<'_>) -> (String, String) {
    let source = input.source;
    let source_line = match source.get("kind").and_then(Value::as_str) {
        Some("working-tree") => {
            let scope = source.get("scope").and_then(Value::as_str).unwrap_or("all");
            let scope_text = if scope == "all" {
                "staged and unstaged"
            } else {
                scope
            };
            format!("Uncommitted local changes ({scope_text}).")
        }
        Some("branch") => {
            let base = source.get("baseRef").and_then(Value::as_str).unwrap_or("");
            let head = source.get("headRef").and_then(Value::as_str).unwrap_or("");
            format!(
                "All work on branch \"{head}\" that is not in \"{base}\". Changes merged in from {base} are already excluded."
            )
        }
        Some("pr") => {
            let number = source.get("number").and_then(Value::as_i64).unwrap_or(0);
            format!("Pull request #{number}.")
        }
        _ => String::new(),
    };

    let previous = previous_walkthrough_section(input.previous_walkthrough);
    let prompt = format!(
        "Reviewing: {source_line}\n\n{}\n{previous}\nChange digest:\n{}",
        sizing(input.file_count, input.hunk_count),
        input.digest_json,
    );

    // The default language adds nothing: the system prompt is already English,
    // so saying so would only spend context restating it.
    let language = input.language.unwrap_or(DEFAULT_LANGUAGE);
    let system = if language == DEFAULT_LANGUAGE {
        SYSTEM.to_string()
    } else {
        format!("{SYSTEM}{}", language_instruction(language))
    };

    (prompt, system)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn prompt_input() -> PromptInput<'static> {
        static SOURCE: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        let source = SOURCE.get_or_init(|| json!({ "kind": "working-tree", "scope": "all" }));
        PromptInput {
            digest_json: "{\"files\":[]}",
            file_count: 1,
            hunk_count: 1,
            source,
            previous_walkthrough: None,
            language: None,
        }
    }

    #[test]
    fn says_nothing_when_the_prompt_language_is_already_the_output_language() {
        let (_, system) = build_prompt(PromptInput {
            language: Some("en"),
            ..prompt_input()
        });
        assert!(!system.contains("Write all prose"));
        // The default matches the explicit English request.
        let (_, default_system) = build_prompt(prompt_input());
        assert_eq!(system, default_system);
    }

    #[test]
    fn asks_for_prose_in_the_chosen_language() {
        let (_, system) = build_prompt(PromptInput {
            language: Some("uk"),
            ..prompt_input()
        });
        assert!(system.contains("Write all prose in Ukrainian"));
    }

    // Aliases are keys the server resolves back to hunk ids and icon/importance
    // are validated against fixed English values, so a translated one is
    // dropped by the normalizer — silently losing an anchor or a style.
    #[test]
    fn holds_back_the_parts_that_are_not_prose() {
        let (_, system) = build_prompt(PromptInput {
            language: Some("ja"),
            ..prompt_input()
        });
        assert!(system.contains("Keep these in English exactly as given"));
        assert!(system.contains("hunk aliases"));
        assert!(system.contains("\"importance\""));
    }

    #[test]
    fn describes_the_source_and_embeds_the_digest() {
        let (prompt, _) = build_prompt(prompt_input());
        assert!(prompt.starts_with("Reviewing: Uncommitted local changes (staged and unstaged)."));
        assert!(prompt.contains("This change has 1 file(s) and 1 reviewable hunk(s)."));
        assert!(prompt.contains("Change digest:\n{\"files\":[]}"));
    }

    #[test]
    fn sizes_the_request_from_the_hunk_count() {
        let (prompt, _) = build_prompt(PromptInput {
            hunk_count: 5,
            ..prompt_input()
        });
        // round(5 / 2.5) = 2 stops, 1 chapter.
        assert!(prompt.contains("Aim for about 2 stop(s) across about 1 chapter(s)"));
        let (prompt, _) = build_prompt(PromptInput {
            hunk_count: 40,
            ..prompt_input()
        });
        // round(40 / 2.5) = 16 stops (the cap), ceil(16 / 3) = 6 chapters (the cap).
        assert!(prompt.contains("Aim for about 16 stop(s) across about 6 chapter(s)"));
    }

    #[test]
    fn includes_the_previous_walkthrough_only_when_it_has_chapters() {
        let previous = json!({
            "title": "Old review",
            "chapters": [{
                "title": "Data",
                "blurb": "shape first",
                "stops": [{ "title": "Field", "prose": "Adds a field." }]
            }]
        });
        let (prompt, _) = build_prompt(PromptInput {
            previous_walkthrough: Some(&previous),
            ..prompt_input()
        });
        assert!(prompt.contains("A previous walkthrough of an earlier state"));
        assert!(prompt.contains("Previous walkthrough — \"Old review\":"));
        assert!(prompt.contains("- Data — shape first"));
        assert!(prompt.contains("  - Field: Adds a field."));
        // Anchors are deliberately absent.
        assert!(!prompt.contains("hunkIds"));

        let (prompt, _) = build_prompt(PromptInput {
            previous_walkthrough: Some(&json!({ "title": "x", "chapters": [] })),
            ..prompt_input()
        });
        assert!(!prompt.contains("A previous walkthrough of an earlier state"));
    }

    #[test]
    fn describes_branch_and_pr_sources() {
        let branch = json!({ "kind": "branch", "baseRef": "main", "headRef": "feature" });
        let (prompt, _) = build_prompt(PromptInput {
            source: &branch,
            ..prompt_input()
        });
        assert!(prompt.starts_with(
            "Reviewing: All work on branch \"feature\" that is not in \"main\". Changes merged in from main are already excluded."
        ));

        let pr = json!({ "kind": "pr", "number": 2122 });
        let (prompt, _) = build_prompt(PromptInput {
            source: &pr,
            ..prompt_input()
        });
        assert!(prompt.starts_with("Reviewing: Pull request #2122."));
    }
}
