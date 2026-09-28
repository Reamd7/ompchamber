/**
 * OMPChamber 托管工具的动作目录（action catalog）与命名空间解析。
 *
 * 本模块是三个托管工具（ompchamber / ompchamber_web /
 * ompchamber_memory）共用的动作清单：每个工具暴露哪些动作、每个动作
 * 对模型的一行描述，以及模型丢掉命名空间前缀时（如把 memory.read 说
 * 成 read）的宽容解析 resolveAgentToolAction。OMPCHAMBER_ALL_ACTIONS
 * 汇总全部动作，供回调路由统一分发与校验。
 */
/**
 * Two capabilities, two tools.
 *
 * Controlling sessions and driving a page are different intents, and a single
 * tool description covering both is vaguer than either — which is how a model
 * ends up calling the wrong one. Separate tools also mean turning one off
 * removes it entirely, parameters included, rather than leaving its inputs
 * visible in a shared schema.
 */
/**
 * 中文补充：两种能力拆成两个工具。控制会话与驱动页面是不同的意图，
 * 一份覆盖两者的工具描述必然比任何一份都含糊——模型于是会调错工具。
 * 拆开后，关掉其中一个就是把它连同参数一起整体移除，而不是在共享
 * schema 里留下看得见却用不了的输入。
 */
export const OMPCHAMBER_CONTROL_ACTION_DEFINITIONS = Object.freeze([
  { action: 'projects.list', title: 'List configured projects', description: 'List configured projects; no parameters' },
  { action: 'models.list', title: 'Show model preferences', description: 'Show default, favorite, and recent model preferences; no parameters' },
  { action: 'session.list', title: 'List sessions', description: 'List sessions; optional directory, limit (default 10), all, or withStatus' },
  { action: 'session.create', title: 'Create a session', description: 'Create a session in the current directory by default; prompt is optional' },
  { action: 'session.send', title: 'Send a prompt', description: 'Send a new prompt to sessionId; scope with projectId or directory' },
  { action: 'session.fork', title: 'Fork a session', description: 'Fork sessionId; messageId selects the boundary; prompt is optional' },
  { action: 'session.status', title: 'Check session status', description: 'Check sessionId status; directory defaults to the current session' },
  { action: 'session.messages', title: 'Read session messages', description: 'Read text-only messages and current sessionStatus for sessionId; directory and limit 10 are defaults' },
  { action: 'schedule.status', title: 'Check scheduler status', description: 'Check scheduler status; no parameters', agentExposed: false },
  { action: 'schedule.list', title: 'List scheduled tasks', description: 'List tasks and scheduler status; scope with projectId or directory' },
  { action: 'schedule.create', title: 'Create a scheduled task', description: 'Create task; requires name, prompt, model, and one schedule selector' },
  { action: 'schedule.run', title: 'Run a scheduled task', description: 'Run taskId; scope with projectId or directory' },
  { action: 'schedule.delete', title: 'Delete a scheduled task', description: 'Delete taskId; scope with projectId or directory' },
  { action: 'schedule.toggle', title: 'Enable or disable a scheduled task', description: 'Enable or disable taskId; requires the disabled boolean' },
]);

/** 控制动作名数组（内部派生自定义表）。 */
const OMPCHAMBER_CONTROL_ACTIONS = Object.freeze(
  OMPCHAMBER_CONTROL_ACTION_DEFINITIONS.map(({ action }) => action),
);

/** 控制工具中暴露给 agent 的动作定义（agentExposed 未显式置 false 的项）。 */
export const OMPCHAMBER_AGENT_TOOL_ACTION_DEFINITIONS = Object.freeze(
  OMPCHAMBER_CONTROL_ACTION_DEFINITIONS.filter(({ agentExposed }) => agentExposed !== false),
);

/** agent 工具可用的控制动作名数组。 */
export const OMPCHAMBER_AGENT_TOOL_ACTIONS = Object.freeze(
  OMPCHAMBER_AGENT_TOOL_ACTION_DEFINITIONS.map(({ action }) => action),
);

/** web（浏览器面板）工具的动作定义与各自的一句话参数说明。 */
export const OMPCHAMBER_WEB_ACTION_DEFINITIONS = Object.freeze([
  { action: 'browser.open', title: 'Open a page in the browser panel', description: 'Open url in the in-app browser panel; use it to look at the running app. Set viewport to mobile, tablet or desktop to lay the page out at that size' },
  { action: 'browser.snapshot', title: 'Read the open page', description: 'Read the open page: url, title, visible text, and interactive elements with the selectors the other browser actions accept. Pass selector to read only that part of a long page. Reports any errors the page logged' },
  { action: 'browser.click', title: 'Click on the open page', description: 'Click an element; give selector, or text to match a link or button by its visible label' },
  { action: 'browser.type', title: 'Type into the open page', description: 'Type value into the field matched by selector; set submit to press Enter afterwards' },
  { action: 'browser.scroll', title: 'Scroll the open page', description: 'Scroll the page; direction is up, down, top, or bottom, or pass selector to bring one element into view' },
  { action: 'browser.back', title: 'Go back in the browser panel', description: 'Return to the previous page in this tab; no parameters' },
  { action: 'browser.forward', title: 'Go forward in the browser panel', description: 'Move forward again in this tab; no parameters' },
  { action: 'browser.inspect', title: 'Read how an element renders', description: 'Read the computed styles of the element matched by selector — colours, fonts, spacing, borders — as the page actually renders them' },
  { action: 'browser.capture', title: 'Save a screenshot of the page', description: 'Save what is currently visible in the browser panel as an image file in the project and return its path, so a change can be shown rather than described. Pass label to name it (for example before-fix); the result reports the page, layout and path to reference in your answer' },
  { action: 'browser.resize', title: 'Change the page viewport', description: 'Lay the open page out at a different size; viewport is mobile, tablet, desktop, or fill to use the whole panel' },
]);

/** web 工具的动作名数组。 */
export const OMPCHAMBER_WEB_ACTIONS = Object.freeze(
  OMPCHAMBER_WEB_ACTION_DEFINITIONS.map(({ action }) => action),
);

/**
 * Memory is its own tool for the same reason web is: remembering across
 * sessions is a distinct intent from controlling one, and a shared description
 * would blur both. It also has to switch off cleanly and completely, which a
 * shared schema cannot do.
 *
 * The session already carries an index of stored titles, so the descriptions
 * push the model toward reading one entry it can already see rather than
 * listing everything again — and toward reading it at all, since a title that
 * reads as a complete fact is exactly the one whose conditions get lost.
 */
/**
 * 中文补充：memory 单独成工具的理由与 web 相同：跨会话记忆与会话
 * 控制是两种意图，共享描述会把两边都讲含糊，而且它必须能被干净地
 * 整体关闭——共享 schema 做不到。会话本就带着已存标题的索引，所以
 * 描述刻意引导模型去读一条它已经看得见的条目（标题读起来像完整事实
 * 的，恰恰是适用条件最容易被丢掉的），而不是再列一遍全部。
 */
export const OMPCHAMBER_MEMORY_ACTION_DEFINITIONS = Object.freeze([
  { action: 'memory.read', title: 'Read a stored memory', description: 'Read the full text of one memory listed in the session index. The index shows titles only, and a title omits the conditions that decide how the memory applies, so read before acting rather than working from the title. Requires title (as the index spells it) or memoryId; scope is optional and both stores are searched without it' },
  { action: 'memory.list', title: 'List stored memories', description: 'List stored memory titles when the session index is missing or stale; scope is global, project, or both (default)' },
  { action: 'memory.save', title: 'Remember something', description: 'Store a durable fact, preference, or reference; requires title and body, plus scope global (about the user) or project (about this codebase). Restating something already stored updates it. Do not store secrets, one-off task state, or anything the user asked you not to keep' },
  { action: 'memory.delete', title: 'Forget a memory', description: 'Delete a memory that turned out to be wrong or obsolete; requires memoryId and scope' },
]);

/** memory 工具的动作名数组。 */
export const OMPCHAMBER_MEMORY_ACTIONS = Object.freeze(
  OMPCHAMBER_MEMORY_ACTION_DEFINITIONS.map(({ action }) => action),
);

/**
 * Which actions each managed tool may ask for.
 *
 * The callback needs this because models routinely drop the namespace: asked
 * for `memory.read` from a tool already called `ompchamber_memory`, they send
 * `read`, since the tool's own name appears to have said "memory" already. The
 * name is unambiguous inside one tool's action set even when it is not across
 * all of them (`delete` belongs to both schedule and memory), so resolution
 * starts from the tool that asked.
 */
/**
 * 中文补充：每个托管工具各自可请求的动作集合。回调路由需要它，是
 * 因为模型经常丢掉命名空间：从名为 ompchamber_memory 的工具里要
 * memory.read 时，模型只发 read——工具自己的名字看起来已经说过
 * "memory" 了。一个裸名字在单个工具内无歧义（即使跨工具有歧义，
 * delete 同时属于 schedule 和 memory），所以解析从发起工具出发。
 */
const ACTIONS_BY_TOOL = Object.freeze({
  ompchamber: OMPCHAMBER_AGENT_TOOL_ACTIONS,
  ompchamber_web: OMPCHAMBER_WEB_ACTIONS,
  ompchamber_memory: OMPCHAMBER_MEMORY_ACTIONS,
});

/** 取动作名的命名空间后缀（"memory.read" → "read"；无点则原样返回）。 */
const bareName = (action) => {
  const separator = action.indexOf('.');
  return separator === -1 ? action : action.slice(separator + 1);
};

/** 裸名在候选集合中恰好命中一个动作时返回它，否则（0 个或多个）返回 null。 */
const uniqueMatch = (candidates, requested) => {
  const matches = candidates.filter((candidate) => bareName(candidate) === requested);
  return matches.length === 1 ? matches[0] : null;
};

/**
 * The canonical action for what a tool asked, or the reason it could not be
 * resolved. The reason lists what the tool can actually do: an error that only
 * says "unsupported" leaves the model to guess again, which is how one wrong
 * name becomes three.
 */
/**
 * 中文补充：返回发起工具实际想调用的规范动作名，或无法解析的原因。
 * 原因里列出该工具真正可用的动作：只说"不支持"会让模型再猜一次，
 * 一个错名字就这样变成三个。
 */
export const resolveAgentToolAction = (requested, toolName) => {
  const value = typeof requested === 'string' ? requested.trim() : '';
  const scoped = ACTIONS_BY_TOOL[toolName] ?? null;
  const known = scoped ?? OMPCHAMBER_ALL_ACTIONS;

  if (value && known.includes(value)) {
    return { action: value };
  }
  if (value) {
    const resolved = uniqueMatch(known, value)
      // A tool that did not identify itself still gets the benefit when the
      // bare name means only one thing across every action.
      ?? (scoped ? null : uniqueMatch(OMPCHAMBER_ALL_ACTIONS, value));
    if (resolved) {
      return { action: resolved };
    }
  }

  return {
    error: `Unsupported OMPChamber action: ${value || 'missing'}. Use one of: ${known.join(', ')}`,
  };
};

/** Everything the callback route will dispatch, whichever tool asked. */
/** 中文补充：回调路由会分发的全部动作，无论哪个工具发起请求。 */
export const OMPCHAMBER_ALL_ACTIONS = Object.freeze([
  ...OMPCHAMBER_CONTROL_ACTIONS,
  ...OMPCHAMBER_WEB_ACTIONS,
  ...OMPCHAMBER_MEMORY_ACTIONS,
]);
