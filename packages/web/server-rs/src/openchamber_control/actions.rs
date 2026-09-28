//! Port of `server/lib/openchamber-control/actions.js` — the fixed action
//! allowlist shared by the CLI HTTP route, the managed `ompchamber` agent
//! tool, the browser tool, and the memory tool, plus the tool-scoped
//! resolver that accepts a bare action name when the calling tool's own
//! name already supplies the namespace.
//!
//! Two capabilities, two tools (actions.js header): controlling sessions
//! and driving a page are different intents, so separate tools — and
//! separate action sets — keep each description precise. `schedule.status`
//! is CLI-only (`agent_exposed: false`).
//! （中文说明）本文件是 `server/lib/openchamber-control/actions.js` 的
//! 移植：固定动作白名单，供 CLI HTTP 路由、受管 `ompchamber` agent 工具、
//! 浏览器工具与记忆工具共享；另含按工具限定作用域的 resolver——当调用
//! 工具自身的名字已经给出命名空间时，允许只写裸 action 名。

/// One entry of the action allowlist. `agent_exposed` marks CLI-only
/// actions the managed agent tool must not offer (JS `agentExposed`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionDefinition {
    /// 完整 action 名（含命名空间，如 `session.fork`）。
    pub action: &'static str,
    /// 展示给用户的短标题。
    pub title: &'static str,
    /// 提供给模型的参数说明。
    pub description: &'static str,
    /// 是否对受管 agent 工具暴露（JS `agentExposed`；`false` 为 CLI-only）。
    pub agent_exposed: bool,
}

/// 白名单条目的紧凑构造器，让定义表每行的四个字段一目了然。
const fn def(
    action: &'static str,
    title: &'static str,
    description: &'static str,
    agent_exposed: bool,
) -> ActionDefinition {
    ActionDefinition {
        action,
        title,
        description,
        agent_exposed,
    }
}

/// `OMPCHAMBER_CONTROL_ACTION_DEFINITIONS`.
/// 控制面（项目/模型/会话/计划任务）动作定义表，含 CLI-only 的
/// `schedule.status`（`agent_exposed: false`）。
pub const CONTROL_ACTION_DEFINITIONS: &[ActionDefinition] = &[
    def(
        "projects.list",
        "List configured projects",
        "List configured projects; no parameters",
        true,
    ),
    def(
        "models.list",
        "Show model preferences",
        "Show default, favorite, and recent model preferences; no parameters",
        true,
    ),
    def(
        "session.list",
        "List sessions",
        "List sessions; optional directory, limit (default 10), all, or withStatus",
        true,
    ),
    def(
        "session.create",
        "Create a session",
        "Create a session in the current directory by default; prompt is optional",
        true,
    ),
    def(
        "session.send",
        "Send a prompt",
        "Send a new prompt to sessionId; scope with projectId or directory",
        true,
    ),
    def(
        "session.fork",
        "Fork a session",
        "Fork sessionId; messageId selects the boundary; prompt is optional",
        true,
    ),
    def(
        "session.status",
        "Check session status",
        "Check sessionId status; directory defaults to the current session",
        true,
    ),
    def(
        "session.messages",
        "Read session messages",
        "Read text-only messages and current sessionStatus for sessionId; directory and limit 10 are defaults",
        true,
    ),
    def(
        "schedule.status",
        "Check scheduler status",
        "Check scheduler status; no parameters",
        false,
    ),
    def(
        "schedule.list",
        "List scheduled tasks",
        "List tasks and scheduler status; scope with projectId or directory",
        true,
    ),
    def(
        "schedule.create",
        "Create a scheduled task",
        "Create task; requires name, prompt, model, and one schedule selector",
        true,
    ),
    def(
        "schedule.run",
        "Run a scheduled task",
        "Run taskId; scope with projectId or directory",
        true,
    ),
    def(
        "schedule.delete",
        "Delete a scheduled task",
        "Delete taskId; scope with projectId or directory",
        true,
    ),
    def(
        "schedule.toggle",
        "Enable or disable a scheduled task",
        "Enable or disable taskId; requires the disabled boolean",
        true,
    ),
];

/// `OMPCHAMBER_CONTROL_ACTIONS` (private in JS; every control action,
/// including the CLI-only `schedule.status`).
/// 全部控制动作名（JS 中为私有；包含 CLI-only 的 `schedule.status`）。
pub const CONTROL_ACTIONS: &[&str] = &[
    "projects.list",
    "models.list",
    "session.list",
    "session.create",
    "session.send",
    "session.fork",
    "session.status",
    "session.messages",
    "schedule.status",
    "schedule.list",
    "schedule.create",
    "schedule.run",
    "schedule.delete",
    "schedule.toggle",
];

/// `OMPCHAMBER_AGENT_TOOL_ACTIONS` — control actions the managed agent
/// tool may offer (`agentExposed !== false`).
/// 受管 agent 工具可提供的控制动作（`agent_exposed != false` 的子集）。
pub const AGENT_TOOL_ACTIONS: &[&str] = &[
    "projects.list",
    "models.list",
    "session.list",
    "session.create",
    "session.send",
    "session.fork",
    "session.status",
    "session.messages",
    "schedule.list",
    "schedule.create",
    "schedule.run",
    "schedule.delete",
    "schedule.toggle",
];

/// `OMPCHAMBER_WEB_ACTION_DEFINITIONS`.
/// 内嵌浏览器面板动作定义表（open、snapshot、click 等页面操作）。
pub const WEB_ACTION_DEFINITIONS: &[ActionDefinition] = &[
    def(
        "browser.open",
        "Open a page in the browser panel",
        "Open url in the in-app browser panel; use it to look at the running app. Set viewport to mobile, tablet or desktop to lay the page out at that size",
        true,
    ),
    def(
        "browser.snapshot",
        "Read the open page",
        "Read the open page: url, title, visible text, and interactive elements with the selectors the other browser actions accept. Pass selector to read only that part of a long page. Reports any errors the page logged",
        true,
    ),
    def(
        "browser.click",
        "Click on the open page",
        "Click an element; give selector, or text to match a link or button by its visible label",
        true,
    ),
    def(
        "browser.type",
        "Type into the open page",
        "Type value into the field matched by selector; set submit to press Enter afterwards",
        true,
    ),
    def(
        "browser.scroll",
        "Scroll the open page",
        "Scroll the page; direction is up, down, top, or bottom, or pass selector to bring one element into view",
        true,
    ),
    def(
        "browser.back",
        "Go back in the browser panel",
        "Return to the previous page in this tab; no parameters",
        true,
    ),
    def(
        "browser.forward",
        "Go forward in the browser panel",
        "Move forward again in this tab; no parameters",
        true,
    ),
    def(
        "browser.inspect",
        "Read how an element renders",
        "Read the computed styles of the element matched by selector — colours, fonts, spacing, borders — as the page actually renders them",
        true,
    ),
    def(
        "browser.capture",
        "Save a screenshot of the page",
        "Save what is currently visible in the browser panel as an image file in the project and return its path, so a change can be shown rather than described. Pass label to name it (for example before-fix); the result reports the page, layout and path to reference in your answer",
        true,
    ),
    def(
        "browser.resize",
        "Change the page viewport",
        "Lay the open page out at a different size; viewport is mobile, tablet, desktop, or fill to use the whole panel",
        true,
    ),
];

/// `OMPCHAMBER_WEB_ACTIONS`.
/// 全部浏览器动作名，与 `WEB_ACTION_DEFINITIONS` 一一对应。
pub const WEB_ACTIONS: &[&str] = &[
    "browser.open",
    "browser.snapshot",
    "browser.click",
    "browser.type",
    "browser.scroll",
    "browser.back",
    "browser.forward",
    "browser.inspect",
    "browser.capture",
    "browser.resize",
];

/// `OMPCHAMBER_MEMORY_ACTION_DEFINITIONS`.
/// 记忆存取动作定义表（read、list、save、delete）。
pub const MEMORY_ACTION_DEFINITIONS: &[ActionDefinition] = &[
    def(
        "memory.read",
        "Read a stored memory",
        "Read the full text of one memory listed in the session index. The index shows titles only, and a title omits the conditions that decide how the memory applies, so read before acting rather than working from the title. Requires title (as the index spells it) or memoryId; scope is optional and both stores are searched without it",
        true,
    ),
    def(
        "memory.list",
        "List stored memories",
        "List stored memory titles when the session index is missing or stale; scope is global, project, or both (default)",
        true,
    ),
    def(
        "memory.save",
        "Remember something",
        "Store a durable fact, preference, or reference; requires title and body, plus scope global (about the user) or project (about this codebase). Restating something already stored updates it. Do not store secrets, one-off task state, or anything the user asked you not to keep",
        true,
    ),
    def(
        "memory.delete",
        "Forget a memory",
        "Delete a memory that turned out to be wrong or obsolete; requires memoryId and scope",
        true,
    ),
];

/// `OMPCHAMBER_MEMORY_ACTIONS`.
/// 全部记忆动作名。
pub const MEMORY_ACTIONS: &[&str] = &["memory.read", "memory.list", "memory.save", "memory.delete"];

/// `OMPCHAMBER_ALL_ACTIONS` — everything the callback route dispatches,
/// whichever tool asked.
/// 回调路由可派发的全部动作：控制、浏览器、记忆三张表的并集，
/// 不区分发起请求的是哪个工具。
pub const ALL_ACTIONS: &[&str] = &[
    // OMPCHAMBER_CONTROL_ACTIONS
    "projects.list",
    "models.list",
    "session.list",
    "session.create",
    "session.send",
    "session.fork",
    "session.status",
    "session.messages",
    "schedule.status",
    "schedule.list",
    "schedule.create",
    "schedule.run",
    "schedule.delete",
    "schedule.toggle",
    // OMPCHAMBER_WEB_ACTIONS
    "browser.open",
    "browser.snapshot",
    "browser.click",
    "browser.type",
    "browser.scroll",
    "browser.back",
    "browser.forward",
    "browser.inspect",
    "browser.capture",
    "browser.resize",
    // OMPCHAMBER_MEMORY_ACTIONS
    "memory.read",
    "memory.list",
    "memory.save",
    "memory.delete",
];

/// `OMPCHAMBER_AGENT_TOOL_ACTION_DEFINITIONS` — control definitions with
/// `agent_exposed != false`.
/// 过滤出 `agent_exposed != false` 的控制动作定义，供受管 agent 工具
/// 生成自己的描述。
pub fn agent_tool_action_definitions() -> Vec<&'static ActionDefinition> {
    CONTROL_ACTION_DEFINITIONS
        .iter()
        .filter(|definition| definition.agent_exposed)
        .collect()
}

/// `ACTIONS_BY_TOOL` — which actions each managed tool may ask for. Models
/// routinely drop the namespace (`read` from `ompchamber_memory`), and the
/// bare name is unambiguous inside one tool's set even when it is not
/// across all of them (`delete` belongs to both schedule and memory).
/// 每个受管工具可请求的动作集合。模型经常省略命名空间（在
/// `ompchamber_memory` 里只写 `read`），而裸名在单个工具的集合内是明确的
/// ——即使跨工具不明确（`delete` 同时属于 schedule 与 memory）。
/// 未识别的工具名返回 `None`。
pub fn actions_for_tool(tool_name: Option<&str>) -> Option<&'static [&'static str]> {
    match tool_name {
        Some("ompchamber") => Some(AGENT_TOOL_ACTIONS),
        Some("ompchamber_web") => Some(WEB_ACTIONS),
        Some("ompchamber_memory") => Some(MEMORY_ACTIONS),
        _ => None,
    }
}

/// `bareName` — everything before the first separator is the namespace.
/// 去掉第一个分隔符之前的命名空间部分，得到裸名。
fn bare_name(action: &str) -> &str {
    match action.find('.') {
        Some(separator) => &action[separator + 1..],
        None => action,
    }
}

/// `uniqueMatch` — the one candidate whose bare name is `requested`.
/// 候选中裸名恰为 `requested` 的唯一条目；撞名（多于一个）返回 `None`，
/// 宁可不解析也不猜。
fn unique_match<'a>(candidates: &[&'a str], requested: &str) -> Option<&'a str> {
    let mut matches = candidates
        .iter()
        .filter(|candidate| bare_name(candidate) == requested);
    let first = matches.next()?;
    matches.next().map_or(Some(first), |_| None)
}

/// `resolveAgentToolAction` — the canonical action for what a tool asked,
/// or the reason it could not be resolved. The reason lists what the tool
/// can actually do: an error that only says "unsupported" leaves the model
/// to guess again.
/// 把工具请求的 action 归一为规范全名：先在调用工具的动作集内做全名匹配，
/// 再做裸名唯一匹配（未表明身份的调用者退到全量表）。无法解析时返回的
/// 原因字符串会列出该工具实际能做的动作——只回一句 "unsupported" 会让
/// 模型继续瞎猜。
pub fn resolve_agent_tool_action(
    requested: Option<&str>,
    tool_name: Option<&str>,
) -> Result<&'static str, String> {
    let value = requested.unwrap_or("").trim();
    let scoped = actions_for_tool(tool_name);
    let known = scoped.unwrap_or(ALL_ACTIONS);

    if !value.is_empty()
        && let Some(action) = known.iter().find(|candidate| **candidate == value)
    {
        return Ok(action);
    }
    if !value.is_empty() {
        // A tool that did not identify itself still gets the benefit when
        // the bare name means only one thing across every action.
        let resolved = unique_match(known, value).or_else(|| {
            if scoped.is_none() {
                unique_match(ALL_ACTIONS, value)
            } else {
                None
            }
        });
        if let Some(action) = resolved {
            return Ok(action);
        }
    }

    Err(format!(
        "Unsupported OMPChamber action: {}. Use one of: {}",
        if value.is_empty() { "missing" } else { value },
        known.join(", ")
    ))
}

/// 动作解析与白名单组合的单元测试（对齐 JS 版 action-resolution.test.js）。
#[cfg(test)]
mod tests {
    use super::*;

    // action-resolution.test.js — both failure cases there came from one
    // real conversation (bare `read`, then bare `get`).
    /// 验证：记忆工具内的裸名 `read`/`save` 解析为 `memory.read`/`memory.save`。
    #[test]
    fn resolves_a_bare_action_inside_the_calling_tool() {
        assert_eq!(
            resolve_agent_tool_action(Some("read"), Some("ompchamber_memory")),
            Ok("memory.read")
        );
        assert_eq!(
            resolve_agent_tool_action(Some("save"), Some("ompchamber_memory")),
            Ok("memory.save")
        );
    }

    /// 验证：`delete` 这类跨工具重名的裸名，在各自工具作用域内解析到各自动作。
    #[test]
    fn resolves_a_bare_name_ambiguous_only_across_tools() {
        assert_eq!(
            resolve_agent_tool_action(Some("delete"), Some("ompchamber_memory")),
            Ok("memory.delete")
        );
        assert_eq!(
            resolve_agent_tool_action(Some("delete"), Some("ompchamber")),
            Ok("schedule.delete")
        );
    }

    /// 验证：完整限定名（含命名空间）原样通过，不做改写。
    #[test]
    fn keeps_a_fully_qualified_action_as_it_is() {
        assert_eq!(
            resolve_agent_tool_action(Some("memory.read"), Some("ompchamber_memory")),
            Ok("memory.read")
        );
    }

    /// 验证：解析不会越出请求工具的动作集——记忆工具请求 `open` 必须失败，
    /// 而不是去驱动浏览器。
    #[test]
    fn does_not_reach_outside_the_tool_that_asked() {
        // The memory tool asking for `open` must fail, not drive the browser.
        assert!(resolve_agent_tool_action(Some("open"), Some("ompchamber_memory")).is_err());
    }

    /// 验证：未表明身份的调用者仍可解析全局唯一的裸名（如 `snapshot`）。
    #[test]
    fn unidentified_caller_resolves_unique_bare_names() {
        assert_eq!(
            resolve_agent_tool_action(Some("snapshot"), None),
            Ok("browser.snapshot")
        );
    }

    /// 验证：未表明身份的调用者遇到多工具共享的裸名（如 `list`）时拒绝解析。
    #[test]
    fn unidentified_caller_refuses_shared_bare_names() {
        assert!(resolve_agent_tool_action(Some("list"), None).is_err());
    }

    /// 验证：无法解析时报错只列出调用工具自己的动作，不掺入其它工具的。
    #[test]
    fn unresolvable_action_names_the_calling_tools_actions() {
        let error = resolve_agent_tool_action(Some("get"), Some("ompchamber_memory"))
            .expect_err("get must not resolve");
        assert!(error.contains("memory.read"), "error: {error}");
        assert!(error.contains("memory.save"), "error: {error}");
        // Listing every action of every tool would bury the four that apply.
        assert!(!error.contains("browser.open"), "error: {error}");
    }

    /// 验证：action 缺失时按 "missing" 报告，而不是尝试解析。
    #[test]
    fn reports_a_missing_action_rather_than_resolving() {
        let error = resolve_agent_tool_action(Some(""), Some("ompchamber_memory"))
            .expect_err("empty must not resolve");
        assert!(error.contains("missing"), "error: {error}");
    }

    /// 验证：未知工具回退到全量动作表，报错里包含所有工具的动作。
    #[test]
    fn unknown_tool_falls_back_to_the_full_action_list() {
        let error = resolve_agent_tool_action(Some("nonsense"), Some("ompchamber_future"))
            .expect_err("nonsense must not resolve");
        assert!(error.contains("memory.read"), "error: {error}");
        assert!(error.contains("browser.open"), "error: {error}");
    }

    /// 验证：仅空白字符的请求同样按 "missing" 报告。
    #[test]
    fn whitespace_only_request_reports_missing() {
        let error = resolve_agent_tool_action(Some("  "), Some("ompchamber"))
            .expect_err("blank must not resolve");
        assert!(error.contains("missing"), "error: {error}");
    }

    // Registry composition: the name lists must stay derived from the
    // definitions (JS maps/filters them; the consts here are spelled out).
    /// 验证：各名称列表必须与定义表保持派生一致（JS 用 map/filter 派生，
    /// 这里是显式展开，故需测试锁定）。
    #[test]
    fn name_lists_match_their_definitions() {
        let control: Vec<&str> = CONTROL_ACTION_DEFINITIONS
            .iter()
            .map(|d| d.action)
            .collect();
        assert_eq!(CONTROL_ACTIONS, control.as_slice());

        let agent: Vec<&str> = CONTROL_ACTION_DEFINITIONS
            .iter()
            .filter(|d| d.agent_exposed)
            .map(|d| d.action)
            .collect();
        assert_eq!(AGENT_TOOL_ACTIONS, agent.as_slice());
        assert!(!AGENT_TOOL_ACTIONS.contains(&"schedule.status"));

        let web: Vec<&str> = WEB_ACTION_DEFINITIONS.iter().map(|d| d.action).collect();
        assert_eq!(WEB_ACTIONS, web.as_slice());

        let memory: Vec<&str> = MEMORY_ACTION_DEFINITIONS.iter().map(|d| d.action).collect();
        assert_eq!(MEMORY_ACTIONS, memory.as_slice());

        let mut all: Vec<&str> = Vec::new();
        all.extend_from_slice(CONTROL_ACTIONS);
        all.extend_from_slice(WEB_ACTIONS);
        all.extend_from_slice(MEMORY_ACTIONS);
        assert_eq!(ALL_ACTIONS, all.as_slice());
        assert_eq!(
            agent_tool_action_definitions().len(),
            AGENT_TOOL_ACTIONS.len()
        );
    }
}
