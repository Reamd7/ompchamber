//! Generated OpenCode plugin source and `OPENCODE_CONFIG_CONTENT` merging.
//!
//! Port of the generation half of `server/lib/agent-tool/runtime.js`. The
//! materialized plugin stays JavaScript — it runs inside the managed OpenCode
//! engine — so the template below is the JS `String.raw` template embedded
//! verbatim, with the JS `${…}` substitutions replaced by `@@MARKER@@`s that
//! cannot occur in the substituted content. Serialization of the substituted
//! values mirrors `JSON.stringify` exactly (compact, `"`, `\`, and control
//! characters escaped; non-ASCII kept raw), and parameter maps are kept in
//! declaration order — the same object-key order the JS emits.
//!
//! 中文说明：本文件承载生成侧——把工具定义渲染成在受管 OpenCode 引擎内运行的
//! JavaScript 插件源码，并把插件 URL 合并进 `OPENCODE_CONFIG_CONTENT`。模板为 JS
//! `String.raw` 的逐字嵌入，`@@MARKER@@` 为占位符；被替换值的序列化与 `JSON.stringify`
//! 完全对齐，参数映射保持声明顺序（与 JS 对象键序一致）。

use serde_json::{Map, Value};

use crate::openchamber_control::actions as control_actions;

/// 线上协议版本，与回调信封中的 `schemaVersion` 对应。
pub(crate) const TOOL_SCHEMA_VERSION: u32 = 1;

/// `ompchamber` 控制工具暴露给模型的描述文本（英文原文逐字保留自 JS 生成器）。
pub(crate) const CONTROL_TOOL_DESCRIPTION: &str = "Control OMPChamber projects, sessions, and scheduled tasks on the user's behalf. Sessions and scheduled tasks you create are for the user to follow and interact with; never use this tool to delegate parts of your own current task. Use one action per call. Scope with projectId or directory; omit both to use the current session directory. Session dispatches return immediately by default and you receive no notification when a dispatched session finishes, so never promise to report back on it; the user follows it in OMPChamber; a dispatched session needs no follow-up from you. If the user later asks how it went, use session.messages (add wait to block until it is idle, lastAssistant for just the final answer) — session.send always sends a NEW prompt and never just waits. Set wait only when the user asks or the next step requires the completed result. Session and worktree deletion are unavailable.";

/// `ompchamber_web` 浏览器工具的描述文本（英文原文逐字保留自 JS 生成器）。
pub(crate) const WEB_TOOL_DESCRIPTION: &str = "Look at and interact with a web page in OMPChamber's browser panel, so you can check your own work rather than describing what you expect. Use one action per call. Open a page, snapshot it to read its text and its interactive elements, then click, type or scroll using the selectors the snapshot returned; snapshots also report any errors the page logged. Pass a selector to browser.snapshot to read one part of a long page. browser.inspect returns computed styles when the question is how something renders. Set viewport to check a layout at mobile, tablet or desktop size. The page runs with the user's real logins, so treat what you see as their live session.";

/// `ompchamber_memory` 记忆工具的描述文本（英文原文逐字保留自 JS 生成器）。
pub(crate) const MEMORY_TOOL_DESCRIPTION: &str = "Keep what you learn across sessions, so the user does not have to explain the same thing twice. Use one action per call. The session already lists the titles of what is stored. A title is an abbreviation, not the memory: read the entry with memory.read before acting on it, because titles leave out the conditions and exceptions that decide how the memory applies, and the ones that look self-explanatory hide them most often. Save something only when it will still be true in a later session — a stable preference, a project convention, a decision and its reason, or a hard-won pointer. Do not save one-off task state, anything you can read from the code, secrets or credentials, or anything the user asked you not to keep. Choose the scope deliberately: global is about the user and reaches every project, so put a project's conventions in project scope. What you save is shown to the user as unreviewed until they confirm it, so save plainly and say what you saved when it matters.";

/// `(name, exact JSON schema text)` in `ALL_PARAMETER_PROPERTIES` declaration
/// order — the order the generated plugin emits them in.
/// 中文：条目顺序即生成插件的输出顺序。
const ALL_PARAMETER_PROPERTIES: &[(&str, &str)] = &[
    (
        "projectId",
        r#"{"type":"string","description":"Configured project ID; do not combine with directory"}"#,
    ),
    (
        "directory",
        r#"{"type":"string","description":"Absolute checkout or session directory; defaults to the current session directory"}"#,
    ),
    ("sessionId", r#"{"type":"string"}"#),
    (
        "messageId",
        r#"{"type":"string","description":"Optional fork boundary message ID"}"#,
    ),
    ("taskId", r#"{"type":"string"}"#),
    ("title", r#"{"type":"string"}"#),
    ("prompt", r#"{"type":"string"}"#),
    (
        "model",
        r#"{"type":"string","description":"Model in provider/model format. When the user names no model: for session.create pick a suitable one from models.list favorites or recents (omit if there are none); for send and fork omit it — the session reuses its previous model"}"#,
    ),
    (
        "agent",
        r#"{"type":"string","description":"OpenCode agent name; new sessions default to the build agent and existing sessions keep their previous one. Set only when the user explicitly requests a different agent"}"#,
    ),
    (
        "variant",
        r#"{"type":"string","description":"Model variant; use only when the user explicitly requests it"}"#,
    ),
    (
        "worktree",
        r#"{"type":"string","description":"New worktree name for session.create. Omit by default; use only when the user explicitly asks for an isolated worktree. Uncommitted changes do not carry over into a new worktree"}"#,
    ),
    (
        "branch",
        r#"{"type":"string","description":"Branch name for the new worktree"}"#,
    ),
    (
        "startRef",
        r#"{"type":"string","description":"Git ref used to create the new worktree"}"#,
    ),
    (
        "setUpstream",
        r#"{"type":"boolean","description":"Make the new worktree branch track its upstream"}"#,
    ),
    (
        "goal",
        r#"{"type":"boolean","description":"Run the dispatched prompt in Goal Mode; use only when the user explicitly requests it"}"#,
    ),
    (
        "goalTokenBudget",
        r#"{"type":"integer","minimum":1000,"maximum":100000000,"description":"Goal token budget; requires goal"}"#,
    ),
    (
        "wait",
        r#"{"type":"boolean","description":"Wait for current session activity to become idle. Omit by default; use only when the user asks or the next step requires the completed result"}"#,
    ),
    (
        "timeout",
        r#"{"type":"integer","minimum":1,"maximum":86400,"description":"Wait timeout in seconds (default 600); requires wait"}"#,
    ),
    (
        "lastAssistant",
        r#"{"type":"boolean","description":"Return the last assistant text; create/send/fork require wait"}"#,
    ),
    (
        "limit",
        r#"{"type":"integer","minimum":1,"description":"Maximum sessions or messages to return (default 10)"}"#,
    ),
    (
        "all",
        r#"{"type":"boolean","description":"Include archived sessions or all messages, depending on the action"}"#,
    ),
    (
        "last",
        r#"{"type":"boolean","description":"Return only the last matching session message"}"#,
    ),
    (
        "withStatus",
        r#"{"type":"boolean","description":"Include authoritative status in session.list"}"#,
    ),
    (
        "role",
        r#"{"type":"string","enum":["all","user","assistant"],"description":"Message role filter"}"#,
    ),
    ("name", r#"{"type":"string"}"#),
    (
        "daily",
        r#"{"type":"string","description":"Daily run time in HH:mm format"}"#,
    ),
    (
        "weekly",
        r#"{"type":"string","description":"Comma-separated weekdays; 0=Sunday and 6=Saturday"}"#,
    ),
    (
        "once",
        r#"{"type":"string","description":"One-time run date in YYYY-MM-DD format"}"#,
    ),
    (
        "time",
        r#"{"type":"string","description":"Weekly or one-time run time in HH:mm format"}"#,
    ),
    (
        "cron",
        r#"{"type":"string","description":"Cron expression"}"#,
    ),
    (
        "timezone",
        r#"{"type":"string","description":"IANA timezone"}"#,
    ),
    (
        "disabled",
        r#"{"type":"boolean","description":"true disables and false enables; required for schedule.toggle"}"#,
    ),
    (
        "url",
        r#"{"type":"string","description":"http(s) URL for browser.open"}"#,
    ),
    (
        "selector",
        r#"{"type":"string","description":"CSS selector from a browser.snapshot result"}"#,
    ),
    (
        "text",
        r#"{"type":"string","description":"Visible label to match when no selector is given"}"#,
    ),
    (
        "value",
        r#"{"type":"string","description":"Text to type for browser.type"}"#,
    ),
    (
        "submit",
        r#"{"type":"boolean","description":"Press Enter after typing"}"#,
    ),
    (
        "direction",
        r#"{"type":"string","enum":["up","down","top","bottom"],"description":"Scroll direction for browser.scroll"}"#,
    ),
    (
        "viewport",
        r#"{"type":"string","enum":["mobile","tablet","desktop","fill"],"description":"Page layout size; snapshots report which one is in effect"}"#,
    ),
    (
        "label",
        r#"{"type":"string","description":"Short name for a browser.capture image, such as before-fix"}"#,
    ),
    (
        "body",
        r#"{"type":"string","description":"Full text of the memory; state it so it still makes sense in a session that has none of this conversation"}"#,
    ),
    (
        "scope",
        r#"{"type":"string","enum":["global","project","both"],"description":"global is about the user and applies everywhere; project is about this codebase. both is only valid for memory.list"}"#,
    ),
    (
        "memoryId",
        r#"{"type":"string","description":"Memory ID from a memory.list or memory.read result"}"#,
    ),
    (
        "type",
        r#"{"type":"string","enum":["fact","preference","reference"],"description":"fact is something true, preference is how the user wants work done, reference points at a resource that is hard to find again"}"#,
    ),
];

/// `WEB_PARAMETER_NAMES` — everything the web tool may ask for.
/// 中文：web 工具可能用到的全部输入名。
const WEB_PARAMETER_NAMES: &[&str] = &[
    "url",
    "selector",
    "text",
    "value",
    "submit",
    "direction",
    "viewport",
    "label",
];

/// `MEMORY_ONLY_PARAMETER_NAMES` — names memory alone introduces (`title` is
/// shared with the control tool, so it is not listed here).
/// 中文：仅由 memory 工具引入的参数名（`title` 与控制工具共享，故不在此列）。
const MEMORY_ONLY_PARAMETER_NAMES: &[&str] = &["body", "scope", "memoryId", "type"];

/// `MEMORY_PARAMETER_OVERRIDES`, applied in place (JS object-spread keeps the
/// original key position for keys that already exist).
/// 中文：仅覆盖已存在的键（JS 对象展开保持原键位）。
fn memory_parameter_override(name: &str) -> Option<&'static str> {
    match name {
        "title" => Some(
            r#"{"type":"string","description":"The memory's title, exactly as the session index lists it. Use this to read an entry you can already see; use memoryId only when a result gave you one"}"#,
        ),
        "scope" => Some(
            r#"{"type":"string","enum":["global","project","both"],"description":"global is about the user and applies everywhere; project is about this codebase. Required for memory.save and memory.delete. Optional for memory.read and memory.list, which search both stores when it is omitted"}"#,
        ),
        _ => None,
    }
}

/// 按名字列表从共享参数表挑出条目，保持声明顺序。
fn pick_parameters(names: &[&str]) -> Vec<(&'static str, &'static str)> {
    ALL_PARAMETER_PROPERTIES
        .iter()
        .filter(|(name, _)| names.contains(name))
        .map(|(name, json)| (*name, *json))
        .collect()
}

/// Everything the control tool's actions take: the shared map minus web-only
/// and memory-only inputs.
/// 中文：共享参数表去掉 web 专属与 memory 专属输入。
pub(crate) fn control_parameter_properties() -> Vec<(&'static str, &'static str)> {
    ALL_PARAMETER_PROPERTIES
        .iter()
        .filter(|(name, _)| {
            !WEB_PARAMETER_NAMES.contains(name) && !MEMORY_ONLY_PARAMETER_NAMES.contains(name)
        })
        .map(|(name, json)| (*name, *json))
        .collect()
}

/// web 工具的参数表：按 `WEB_PARAMETER_NAMES` 顺序挑选。
pub(crate) fn web_parameter_properties() -> Vec<(&'static str, &'static str)> {
    pick_parameters(WEB_PARAMETER_NAMES)
}

/// memory 工具的参数表：在共享表基础上叠加 memory 专属覆盖（如 `title`、`scope`）。
pub(crate) fn memory_parameter_properties() -> Vec<(&'static str, &'static str)> {
    pick_parameters(&["body", "scope", "memoryId", "type", "title"])
        .into_iter()
        .map(|(name, json)| (name, memory_parameter_override(name).unwrap_or(json)))
        .collect()
}

/// One generated tool entry. `oneof` holds `(action, description)` pairs and
/// `parameters` holds `(name, JSON text)` pairs in emission order.
/// 中文：`oneof` 与 `parameters` 均按输出顺序排列。
pub(crate) struct ToolSpec {
    /// 工具名（如 `ompchamber`），在生成对象中作为键出现两次。
    pub name: &'static str,
    /// 工具描述常量。
    pub description: &'static str,
    /// 该工具可请求的 action 名列表。
    pub actions: Vec<&'static str>,
    /// `(action, description)` 对，渲染为 `oneof` 枚举约束。
    pub oneof: Vec<(&'static str, &'static str)>,
    /// `(参数名, JSON schema 文本)` 对，渲染为参数对象。
    pub parameters: Vec<(&'static str, &'static str)>,
}

/// The JS `createToolEntry` `String.raw` template, verbatim. Substitution
/// markers: `@@NAME_KEY@@` (object key, appears twice), `@@DESCRIPTION@@`,
/// `@@ACTIONS@@`, `@@ONEOF@@`, `@@PARAMETERS@@`, `@@TITLES@@`, `@@SV@@`
/// (schema version), `@@NAME_STR@@` (JSON-stringified tool name).
/// 中文：占位符在替换内容中不可能出现，可安全做纯文本替换。
const TOOL_ENTRY_TEMPLATE: &str = r#"    @@NAME_KEY@@: {
      description: @@DESCRIPTION@@,
      args: {
        action: { type: "string", enum: @@ACTIONS@@, oneOf: @@ONEOF@@, description: "OMPChamber action to perform" },
        parameters: { type: "object", properties: @@PARAMETERS@@, additionalProperties: false, description: "Inputs for the action; use an empty object when none are needed" },
      },
      async execute(input, context) {
        // Models routinely put the inputs next to the action instead of inside
        // the parameters object, and dropping them there produced a
        // "url is required" error for a call that plainly carried a url. Both
        // shapes are accepted; an explicit parameters object wins on a conflict.
        const { action: requestedAction, parameters, ...flattened } = input ?? {}
        const args = { ...flattened, ...(parameters ?? {}), action: requestedAction }
        const actionTitles = @@TITLES@@
        const title = Object.hasOwn(actionTitles, args.action) ? actionTitles[args.action] : args.action
        context.metadata({
          title,
          metadata: {
            @@NAME_KEY@@: {
              schemaVersion: @@SV@@,
              action: args.action,
              description: title,
            },
          },
        })
        const endpoint = process.env.OMPCHAMBER_AGENT_TOOL_URL
        const token = process.env.OMPCHAMBER_AGENT_TOOL_TOKEN
        const failure = (payload) => ({
          title,
          output: JSON.stringify(payload),
          metadata: { ompchamber: { schemaVersion: @@SV@@, action: args.action, description: title, ok: false } },
        })
        if (!endpoint || !token) {
          return failure({ schemaVersion: @@SV@@, ok: false, action: args.action, error: { message: "OMPChamber managed tool connection is unavailable" } })
        }

        try {
          const response = await fetch(endpoint, {
            method: "POST",
            headers: {
              authorization: "Bearer " + token,
              "content-type": "application/json",
            },
            body: JSON.stringify({ input: args, contextDirectory: context.directory, tool: @@NAME_STR@@ }),
            signal: context.abort,
          })
          const output = await response.text()
          let result = null
          try { result = JSON.parse(output) } catch {}
          const valid = result?.schemaVersion === @@SV@@ && typeof result?.ok === "boolean" && typeof result?.action === "string"
          context.metadata({
            title,
            metadata: {
              @@NAME_KEY@@: {
                schemaVersion: @@SV@@,
                action: args.action,
                description: title,
                ok: valid && result.ok === true,
              },
            },
          })
          if (valid) return { title, output, metadata: { ompchamber: { schemaVersion: @@SV@@, action: args.action, description: title, ok: result.ok === true } } }
          return failure({ schemaVersion: @@SV@@, ok: false, action: args.action, error: { message: "OMPChamber returned an invalid response", kind: "runtime", status: response.status } })
        } catch (error) {
          if (context.abort.aborted) throw error
          return failure({ schemaVersion: @@SV@@, ok: false, action: args.action, error: { message: error instanceof Error ? error.message : String(error), kind: "runtime" } })
        }
      },
    },
"#;

/// `JSON.stringify` parity for the values interpolated into the template.
/// 中文：依赖 serde_json 的转义规则与 `JSON.stringify` 一致（紧凑、非 ASCII 原样）。
fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

/// 渲染一个工具条目：把 actions、oneof、parameters 序列化后替换模板占位符。
pub(crate) fn create_tool_entry(spec: &ToolSpec, titles_json: &str) -> String {
    let actions_json = format!(
        "[{}]",
        spec.actions
            .iter()
            .map(|action| json_string(action))
            .collect::<Vec<_>>()
            .join(",")
    );
    let oneof_json = format!(
        "[{}]",
        spec.oneof
            .iter()
            .map(|(action, description)| format!(
                "{{\"const\":{},\"description\":{}}}",
                json_string(action),
                json_string(description)
            ))
            .collect::<Vec<_>>()
            .join(",")
    );
    let parameters_json = format!(
        "{{{}}}",
        spec.parameters
            .iter()
            .map(|(name, json)| format!("{}:{}", json_string(name), json))
            .collect::<Vec<_>>()
            .join(",")
    );
    TOOL_ENTRY_TEMPLATE
        .replace("@@NAME_KEY@@", spec.name)
        .replace("@@NAME_STR@@", &json_string(spec.name))
        .replace("@@DESCRIPTION@@", &json_string(spec.description))
        .replace("@@ACTIONS@@", &actions_json)
        .replace("@@ONEOF@@", &oneof_json)
        .replace("@@PARAMETERS@@", &parameters_json)
        .replace("@@TITLES@@", titles_json)
        .replace("@@SV@@", &TOOL_SCHEMA_VERSION.to_string())
}

/// The `AGENT_TOOL_ACTION_TITLES` map: action → short title across every
/// definition either managed tool may ask for, in declaration order.
/// 中文：按声明顺序合并控制、web、memory 三组定义。
pub(crate) fn action_titles_json() -> String {
    let mut pairs = Vec::new();
    for definition in control_actions::agent_tool_action_definitions() {
        pairs.push((definition.action, definition.title));
    }
    for definition in control_actions::WEB_ACTION_DEFINITIONS {
        pairs.push((definition.action, definition.title));
    }
    for definition in control_actions::MEMORY_ACTION_DEFINITIONS {
        pairs.push((definition.action, definition.title));
    }
    format!(
        "{{{}}}",
        pairs
            .iter()
            .map(|(action, title)| format!("{}:{}", json_string(action), json_string(title)))
            .collect::<Vec<_>>()
            .join(",")
    )
}

/// `createPluginSource` — the specs arrive in fixed order (control, web,
/// memory); only the enabled ones are passed.
/// 中文：输出形如 `export const OMPChamberPlugin = async () => ({ tool: { … } })` 的源码。
pub(crate) fn create_plugin_source(specs: &[ToolSpec]) -> String {
    let titles_json = action_titles_json();
    let mut source = String::from("export const OMPChamberPlugin = async () => ({\n  tool: {\n");
    for spec in specs {
        source.push_str(&create_tool_entry(spec, &titles_json));
    }
    source.push_str("  },\n})\n");
    source
}

/// The tool specs in fixed order, filtered by the include flags.
/// 中文：固定顺序（控制、web、memory），仅返回启用项的完整定义。
pub(crate) fn tool_specs(
    include_control: bool,
    include_web: bool,
    include_memory: bool,
) -> Vec<ToolSpec> {
    let mut specs = Vec::new();
    if include_control {
        specs.push(ToolSpec {
            name: "ompchamber",
            description: CONTROL_TOOL_DESCRIPTION,
            actions: control_actions::AGENT_TOOL_ACTIONS.to_vec(),
            oneof: control_actions::agent_tool_action_definitions()
                .into_iter()
                .map(|definition| (definition.action, definition.description))
                .collect(),
            parameters: control_parameter_properties(),
        });
    }
    if include_web {
        specs.push(ToolSpec {
            name: "ompchamber_web",
            description: WEB_TOOL_DESCRIPTION,
            actions: control_actions::WEB_ACTIONS.to_vec(),
            oneof: control_actions::WEB_ACTION_DEFINITIONS
                .iter()
                .map(|definition| (definition.action, definition.description))
                .collect(),
            parameters: web_parameter_properties(),
        });
    }
    if include_memory {
        specs.push(ToolSpec {
            name: "ompchamber_memory",
            description: MEMORY_TOOL_DESCRIPTION,
            actions: control_actions::MEMORY_ACTIONS.to_vec(),
            oneof: control_actions::MEMORY_ACTION_DEFINITIONS
                .iter()
                .map(|definition| (definition.action, definition.description))
                .collect(),
            parameters: memory_parameter_properties(),
        });
    }
    specs
}

/// `mergePluginConfig`: parse `OPENCODE_CONFIG_CONTENT` as JSONC, drop plugin
/// entries that already reference `plugin_url` (plain URL or `[url, options]`
/// tuple), append the fresh URL, and return the compact JSON.
/// 中文：非对象 JSONC 或 `plugin` 字段非数组时返回错误；结果为紧凑 JSON。
pub(crate) fn merge_plugin_config(
    raw_config: Option<&str>,
    plugin_url: &str,
) -> Result<String, String> {
    let non_empty = raw_config
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.to_string());
    let mut parsed: Map<String, Value> = match non_empty {
        Some(raw) => {
            let options = jsonc_parser::ParseOptions {
                allow_comments: true,
                allow_loose_object_property_names: false,
                allow_trailing_commas: true,
            };
            match jsonc_parser::parse_to_serde_value(&raw, &options) {
                Ok(Some(Value::Object(map))) => map,
                _ => {
                    return Err(
                        "OPENCODE_CONFIG_CONTENT must contain a valid JSON object before OMPChamber can inject its managed tool"
                            .to_string(),
                    )
                }
            }
        }
        None => Map::new(),
    };

    if parsed.contains_key("plugin") && !matches!(parsed.get("plugin"), Some(Value::Array(_))) {
        return Err(
            "OPENCODE_CONFIG_CONTENT plugin must be an array before OMPChamber can inject its managed tool"
                .to_string(),
        );
    }

    let configured = match parsed.get("plugin") {
        Some(Value::Array(entries)) => entries.clone(),
        _ => Vec::new(),
    };
    let mut plugins: Vec<Value> = configured
        .into_iter()
        .filter(|value| match value {
            Value::String(url) => url != plugin_url,
            Value::Array(parts) => {
                !matches!(parts.first(), Some(Value::String(url)) if url == plugin_url)
            }
            _ => true,
        })
        .collect();
    plugins.push(Value::String(plugin_url.to_string()));
    parsed.insert("plugin".to_string(), Value::Array(plugins));

    serde_json::to_string(&Value::Object(parsed)).map_err(|error| error.to_string())
}
