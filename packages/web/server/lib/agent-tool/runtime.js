/**
 * OMPChamber 托管 agent 工具（managed agent tool）运行时。
 *
 * 职责：按开关把 ompchamber、ompchamber_web、ompchamber_memory 三个
 * OpenCode 插件工具的源码生成为文件写进 dataDir；把插件注入
 * OPENCODE_CONFIG_CONTENT；为每个 OpenCode 子进程签发一次性 bearer
 * token；在本机 loopback 挂载 /api/ompchamber/agent-tool 回调路由，把
 * 插件工具的调用鉴权后转发给共享的 control service。
 *
 * 三个工具的源码出自同一模板（createToolEntry），共用同一套传输、元数
 * 据与失败处理；schema 里的 description 面向模型，是行为契约的一部分。
 */
import { parse as parseJsonc } from 'jsonc-parser';
import { pathToFileURL } from 'node:url';
import {
  OMPCHAMBER_AGENT_TOOL_ACTION_DEFINITIONS,
  OMPCHAMBER_AGENT_TOOL_ACTIONS,
  OMPCHAMBER_MEMORY_ACTION_DEFINITIONS,
  OMPCHAMBER_MEMORY_ACTIONS,
  resolveAgentToolAction,
  OMPCHAMBER_WEB_ACTION_DEFINITIONS,
  OMPCHAMBER_WEB_ACTIONS,
} from '../openchamber-control/actions.js';

/** 工具结果信封的 schema 版本；插件与回调两侧都校验它，防止信封漂移。 */
const TOOL_SCHEMA_VERSION = 1;
// Everything either managed tool may ask for; the agent allowlist stays
// narrower than the full control surface.
// 中文补充：这是两个托管工具可能请求的全部动作；agent 白名单比完整的
// 控制面更窄。
const ACTIONS = new Set([...OMPCHAMBER_AGENT_TOOL_ACTIONS, ...OMPCHAMBER_WEB_ACTIONS, ...OMPCHAMBER_MEMORY_ACTIONS]);
/** action 到短标题的映射，注入插件后用作工具调用的展示标题与元数据。 */
const AGENT_TOOL_ACTION_TITLES = Object.fromEntries(
  [
    ...OMPCHAMBER_AGENT_TOOL_ACTION_DEFINITIONS,
    ...OMPCHAMBER_WEB_ACTION_DEFINITIONS,
    ...OMPCHAMBER_MEMORY_ACTION_DEFINITIONS,
  ].map(({ action, title }) => [action, title]),
);

/**
 * Each tool carries only the inputs its own actions take.
 *
 * A shared parameter object would leave a disabled capability's inputs visible
 * in the other tool's schema, which is both misleading and paid for in context
 * on every call.
 */
/**
 * web 工具独占的参数名（中文补充）。每个工具只携带自己动作真正用到的
 * 输入；共用一份参数对象会让被禁用能力的输入出现在另一工具的 schema
 * 里——既误导，又让每次调用都白付上下文。
 */
const WEB_PARAMETER_NAMES = ['url', 'selector', 'text', 'value', 'submit', 'direction', 'viewport', 'label'];
// `title` is shared with the control tool, so it is not listed here — only the
// names memory alone introduces are kept out of the other schemas.
// 中文补充：title 与控制工具共享（在那里指会话标题），不列在这里；此处
// 只收 memory 独有的参数名，避免混进其它工具的 schema。
const MEMORY_ONLY_PARAMETER_NAMES = ['body', 'scope', 'memoryId', 'type'];
/** memory 工具的参数名 = memory 独有参数 + 与控制工具共享的 title。 */
const MEMORY_PARAMETER_NAMES = [...MEMORY_ONLY_PARAMETER_NAMES, 'title'];

/**
 * `title` is shared with the control tool, where it means a session title, so
 * it carries no description in the shared map. Left undescribed for memory the
 * model has nothing to go on and invents a name for it — `name` was sent
 * repeatedly in practice — so memory states what its own `title` is.
 */
/**
 * memory 工具对共享参数的描述覆盖（中文补充）。title 在共享表里没有
 * 描述（对控制工具它不言自明）；若 memory 也不描述，模型无从得知只能
 * 自己编字段名——实践中反复出现过发 name 的情况，因此 memory 在此声
 * 明自己的 title 与 scope 各是什么。
 */
const MEMORY_PARAMETER_OVERRIDES = {
  title: { type: 'string', description: "The memory's title, exactly as the session index lists it. Use this to read an entry you can already see; use memoryId only when a result gave you one" },
  scope: { type: 'string', enum: ['global', 'project', 'both'], description: 'global is about the user and applies everywhere; project is about this codebase. Required for memory.save and memory.delete. Optional for memory.read and memory.list, which search both stores when it is omitted' },
};

/**
 * 全部参数的 JSON schema 属性总表（参数名 → { type, description, … }）。
 * 各工具的参数表都是从这里按名挑出的子集；description 面向模型，属于
 * 行为契约的一部分。
 */
const ALL_PARAMETER_PROPERTIES = {
  projectId: { type: 'string', description: 'Configured project ID; do not combine with directory' },
  directory: { type: 'string', description: 'Absolute checkout or session directory; defaults to the current session directory' },
  sessionId: { type: 'string' },
  messageId: { type: 'string', description: 'Optional fork boundary message ID' },
  taskId: { type: 'string' },
  title: { type: 'string' },
  prompt: { type: 'string' },
  model: { type: 'string', description: 'Model in provider/model format. When the user names no model: for session.create pick a suitable one from models.list favorites or recents (omit if there are none); for send and fork omit it — the session reuses its previous model' },
  agent: { type: 'string', description: 'OpenCode agent name; new sessions default to the build agent and existing sessions keep their previous one. Set only when the user explicitly requests a different agent' },
  variant: { type: 'string', description: 'Model variant; use only when the user explicitly requests it' },
  worktree: { type: 'string', description: 'New worktree name for session.create. Omit by default; use only when the user explicitly asks for an isolated worktree. Uncommitted changes do not carry over into a new worktree' },
  branch: { type: 'string', description: 'Branch name for the new worktree' },
  startRef: { type: 'string', description: 'Git ref used to create the new worktree' },
  setUpstream: { type: 'boolean', description: 'Make the new worktree branch track its upstream' },
  goal: { type: 'boolean', description: 'Run the dispatched prompt in Goal Mode; use only when the user explicitly requests it' },
  goalTokenBudget: { type: 'integer', minimum: 1000, maximum: 100_000_000, description: 'Goal token budget; requires goal' },
  wait: { type: 'boolean', description: 'Wait for current session activity to become idle. Omit by default; use only when the user asks or the next step requires the completed result' },
  timeout: { type: 'integer', minimum: 1, maximum: 86_400, description: 'Wait timeout in seconds (default 600); requires wait' },
  lastAssistant: { type: 'boolean', description: 'Return the last assistant text; create/send/fork require wait' },
  limit: { type: 'integer', minimum: 1, description: 'Maximum sessions or messages to return (default 10)' },
  all: { type: 'boolean', description: 'Include archived sessions or all messages, depending on the action' },
  last: { type: 'boolean', description: 'Return only the last matching session message' },
  withStatus: { type: 'boolean', description: 'Include authoritative status in session.list' },
  role: { type: 'string', enum: ['all', 'user', 'assistant'], description: 'Message role filter' },
  name: { type: 'string' },
  daily: { type: 'string', description: 'Daily run time in HH:mm format' },
  weekly: { type: 'string', description: 'Comma-separated weekdays; 0=Sunday and 6=Saturday' },
  once: { type: 'string', description: 'One-time run date in YYYY-MM-DD format' },
  time: { type: 'string', description: 'Weekly or one-time run time in HH:mm format' },
  cron: { type: 'string', description: 'Cron expression' },
  timezone: { type: 'string', description: 'IANA timezone' },
  disabled: { type: 'boolean', description: 'true disables and false enables; required for schedule.toggle' },
  url: { type: 'string', description: 'http(s) URL for browser.open' },
  selector: { type: 'string', description: 'CSS selector from a browser.snapshot result' },
  text: { type: 'string', description: 'Visible label to match when no selector is given' },
  value: { type: 'string', description: 'Text to type for browser.type' },
  submit: { type: 'boolean', description: 'Press Enter after typing' },
  direction: { type: 'string', enum: ['up', 'down', 'top', 'bottom'], description: 'Scroll direction for browser.scroll' },
  viewport: { type: 'string', enum: ['mobile', 'tablet', 'desktop', 'fill'], description: 'Page layout size; snapshots report which one is in effect' },
  label: { type: 'string', description: 'Short name for a browser.capture image, such as before-fix' },
  body: { type: 'string', description: 'Full text of the memory; state it so it still makes sense in a session that has none of this conversation' },
  scope: { type: 'string', enum: ['global', 'project', 'both'], description: 'global is about the user and applies everywhere; project is about this codebase. both is only valid for memory.list' },
  memoryId: { type: 'string', description: 'Memory ID from a memory.list or memory.read result' },
  type: { type: 'string', enum: ['fact', 'preference', 'reference'], description: 'fact is something true, preference is how the user wants work done, reference points at a resource that is hard to find again' },
};

/** 按参数名从总表挑出子集，组装某个工具自己的 properties 对象。 */
const pickParameters = (names) => Object.fromEntries(
  Object.entries(ALL_PARAMETER_PROPERTIES).filter(([name]) => names.includes(name)),
);

/** 控制工具参数表：总表剔除 web 与 memory 独有参数后的剩余部分。 */
const CONTROL_PARAMETER_PROPERTIES = pickParameters(
  Object.keys(ALL_PARAMETER_PROPERTIES).filter((name) => (
    !WEB_PARAMETER_NAMES.includes(name) && !MEMORY_ONLY_PARAMETER_NAMES.includes(name)
  )),
);
/** web 工具参数表：固定为 WEB_PARAMETER_NAMES 列出的八个输入。 */
const WEB_PARAMETER_PROPERTIES = pickParameters(WEB_PARAMETER_NAMES);
/** memory 工具参数表：基础子集再叠加 MEMORY_PARAMETER_OVERRIDES 的描述覆盖。 */
const MEMORY_PARAMETER_PROPERTIES = {
  ...pickParameters(MEMORY_PARAMETER_NAMES),
  ...MEMORY_PARAMETER_OVERRIDES,
};

/** 控制工具（ompchamber）给模型看的使用说明，随 schema 一并提供。 */
const CONTROL_TOOL_DESCRIPTION = "Control OMPChamber projects, sessions, and scheduled tasks on the user's behalf. Sessions and scheduled tasks you create are for the user to follow and interact with; never use this tool to delegate parts of your own current task. Use one action per call. Scope with projectId or directory; omit both to use the current session directory. Session dispatches return immediately by default and you receive no notification when a dispatched session finishes, so never promise to report back on it; the user follows it in OMPChamber; a dispatched session needs no follow-up from you. If the user later asks how it went, use session.messages (add wait to block until it is idle, lastAssistant for just the final answer) — session.send always sends a NEW prompt and never just waits. Set wait only when the user asks or the next step requires the completed result. Session and worktree deletion are unavailable.";

/** web 工具（ompchamber_web）给模型看的使用说明，随 schema 一并提供。 */
const WEB_TOOL_DESCRIPTION = "Look at and interact with a web page in OMPChamber's browser panel, so you can check your own work rather than describing what you expect. Use one action per call. Open a page, snapshot it to read its text and its interactive elements, then click, type or scroll using the selectors the snapshot returned; snapshots also report any errors the page logged. Pass a selector to browser.snapshot to read one part of a long page. browser.inspect returns computed styles when the question is how something renders. Set viewport to check a layout at mobile, tablet or desktop size. The page runs with the user's real logins, so treat what you see as their live session.";

/** memory 工具（ompchamber_memory）给模型看的使用说明，随 schema 一并提供。 */
const MEMORY_TOOL_DESCRIPTION = "Keep what you learn across sessions, so the user does not have to explain the same thing twice. Use one action per call. The session already lists the titles of what is stored. A title is an abbreviation, not the memory: read the entry with memory.read before acting on it, because titles leave out the conditions and exceptions that decide how the memory applies, and the ones that look self-explanatory hide them most often. Save something only when it will still be true in a later session — a stable preference, a project convention, a decision and its reason, or a hard-won pointer. Do not save one-off task state, anything you can read from the code, secrets or credentials, or anything the user asked you not to keep. Choose the scope deliberately: global is about the user and reaches every project, so put a project's conventions in project scope. What you save is shown to the user as unreviewed until they confirm it, so save plainly and say what you saved when it matters.";

/** 把值规整为 trim 后的非空字符串，否则返回 null（输入防御的通用辅助）。 */
const asNonEmptyString = (value) => {
  if (typeof value !== 'string') return null;
  const trimmed = value.trim();
  return trimmed.length > 0 ? trimmed : null;
};

/**
 * 构造统一的工具结果信封：固定携带 schemaVersion、ok、action；
 * data、error、exitCode 按是否存在条件携带。回调路由与测试都依赖这个
 * 形状。
 */
const createResult = ({ ok, action, data, error, exitCode }) => ({
  schemaVersion: TOOL_SCHEMA_VERSION,
  ok,
  action: action || 'unknown',
  ...(data !== undefined ? { data } : {}),
  ...(error ? { error } : {}),
  ...(Number.isInteger(exitCode) ? { exitCode } : {}),
});

/** 判断地址是否为 IPv4/IPv6 loopback（127.0.0.1、::1 及其映射形式）。 */
const isLoopbackAddress = (value) => {
  const address = typeof value === 'string' ? value.toLowerCase() : '';
  return address === '127.0.0.1'
    || address === '::1'
    || address === '::ffff:127.0.0.1';
};

/**
 * One template, one entry per enabled capability.
 *
 * Both tools speak to the same callback with the same envelope; only the action
 * set, the inputs and the description differ. Generating them from one template
 * keeps the transport, metadata and failure handling identical, which is what
 * the caller depends on.
 */
/**
 * 用同一模板生成一个工具的插件源码条目（中文补充）。两个工具对接同一
 * 个回调、使用同一信封，差别只在动作集、输入与描述；单一模板保证传
 * 输、元数据与失败处理完全一致，这正是调用方所依赖的。
 */
const createToolEntry = ({ name, description, actions, definitions, parameters }) => String.raw`    ${name}: {
      description: ${JSON.stringify(description)},
      args: {
        action: { type: "string", enum: ${JSON.stringify(actions)}, oneOf: ${JSON.stringify(definitions.map((entry) => ({ const: entry.action, description: entry.description })))}, description: "OMPChamber action to perform" },
        parameters: { type: "object", properties: ${JSON.stringify(parameters)}, additionalProperties: false, description: "Inputs for the action; use an empty object when none are needed" },
      },
      async execute(input, context) {
        // Models routinely put the inputs next to the action instead of inside
        // the parameters object, and dropping them there produced a
        // "url is required" error for a call that plainly carried a url. Both
        // shapes are accepted; an explicit parameters object wins on a conflict.
        const { action: requestedAction, parameters, ...flattened } = input ?? {}
        const args = { ...flattened, ...(parameters ?? {}), action: requestedAction }
        const actionTitles = ${JSON.stringify(AGENT_TOOL_ACTION_TITLES)}
        const title = Object.hasOwn(actionTitles, args.action) ? actionTitles[args.action] : args.action
        context.metadata({
          title,
          metadata: {
            ${name}: {
              schemaVersion: ${TOOL_SCHEMA_VERSION},
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
          metadata: { ompchamber: { schemaVersion: ${TOOL_SCHEMA_VERSION}, action: args.action, description: title, ok: false } },
        })
        if (!endpoint || !token) {
          return failure({ schemaVersion: ${TOOL_SCHEMA_VERSION}, ok: false, action: args.action, error: { message: "OMPChamber managed tool connection is unavailable" } })
        }

        try {
          const response = await fetch(endpoint, {
            method: "POST",
            headers: {
              authorization: "Bearer " + token,
              "content-type": "application/json",
            },
            body: JSON.stringify({ input: args, contextDirectory: context.directory, tool: ${JSON.stringify(name)} }),
            signal: context.abort,
          })
          const output = await response.text()
          let result = null
          try { result = JSON.parse(output) } catch {}
          const valid = result?.schemaVersion === ${TOOL_SCHEMA_VERSION} && typeof result?.ok === "boolean" && typeof result?.action === "string"
          context.metadata({
            title,
            metadata: {
              ${name}: {
                schemaVersion: ${TOOL_SCHEMA_VERSION},
                action: args.action,
                description: title,
                ok: valid && result.ok === true,
              },
            },
          })
          if (valid) return { title, output, metadata: { ompchamber: { schemaVersion: ${TOOL_SCHEMA_VERSION}, action: args.action, description: title, ok: result.ok === true } } }
          return failure({ schemaVersion: ${TOOL_SCHEMA_VERSION}, ok: false, action: args.action, error: { message: "OMPChamber returned an invalid response", kind: "runtime", status: response.status } })
        } catch (error) {
          if (context.abort.aborted) throw error
          return failure({ schemaVersion: ${TOOL_SCHEMA_VERSION}, ok: false, action: args.action, error: { message: error instanceof Error ? error.message : String(error), kind: "runtime" } })
        }
      },
    },
`;

/**
 * 拼出完整的 OpenCode 插件源码文件：按 includeControl/includeWeb/
 * includeMemory 把各工具条目组装进 OMPChamberPlugin 导出。
 */
const createPluginSource = ({ includeControl, includeWeb, includeMemory }) => {
  const entries = [];
  if (includeControl) {
    entries.push(createToolEntry({
      name: 'ompchamber',
      description: CONTROL_TOOL_DESCRIPTION,
      actions: OMPCHAMBER_AGENT_TOOL_ACTIONS,
      definitions: OMPCHAMBER_AGENT_TOOL_ACTION_DEFINITIONS,
      parameters: CONTROL_PARAMETER_PROPERTIES,
    }));
  }
  if (includeWeb) {
    entries.push(createToolEntry({
      name: 'ompchamber_web',
      description: WEB_TOOL_DESCRIPTION,
      actions: OMPCHAMBER_WEB_ACTIONS,
      definitions: OMPCHAMBER_WEB_ACTION_DEFINITIONS,
      parameters: WEB_PARAMETER_PROPERTIES,
    }));
  }
  if (includeMemory) {
    entries.push(createToolEntry({
      name: 'ompchamber_memory',
      description: MEMORY_TOOL_DESCRIPTION,
      actions: OMPCHAMBER_MEMORY_ACTIONS,
      definitions: OMPCHAMBER_MEMORY_ACTION_DEFINITIONS,
      parameters: MEMORY_PARAMETER_PROPERTIES,
    }));
  }

  return `export const OMPChamberPlugin = async () => ({
  tool: {
${entries.join('')}  },
})
`;
};

/**
 * 把插件 URL 合并进用户提供的 OPENCODE_CONFIG_CONTENT（JSONC）：保留已
 * 配置的其它 plugin 条目（剔除指向同一 URL 的旧条目），把本插件追加到
 * 末尾。配置不是合法 JSON 对象、或 plugin 字段不是数组时直接抛错，绝不
 * 静默丢弃用户配置。返回序列化后的 JSON 字符串。
 */
const mergePluginConfig = (rawConfig, pluginUrl) => {
  const errors = [];
  const parsed = asNonEmptyString(rawConfig) ? parseJsonc(rawConfig, errors, { allowTrailingComma: true }) : {};
  if (errors.length > 0 || !parsed || typeof parsed !== 'object' || Array.isArray(parsed)) {
    throw new Error('OPENCODE_CONFIG_CONTENT must contain a valid JSON object before OMPChamber can inject its managed tool');
  }
  if (parsed.plugin !== undefined && !Array.isArray(parsed.plugin)) {
    throw new Error('OPENCODE_CONFIG_CONTENT plugin must be an array before OMPChamber can inject its managed tool');
  }
  const configured = Array.isArray(parsed.plugin) ? parsed.plugin : [];
  parsed.plugin = [
    ...configured.filter((value) => value !== pluginUrl && (!Array.isArray(value) || value[0] !== pluginUrl)),
    pluginUrl,
  ];
  return JSON.stringify(parsed);
};

/**
 * 创建托管 agent 工具运行时。依赖：crypto、fsPromises、path、dataDir、
 * getActivePort（当前监听端口）、executeAction（共享 control service
 * 的动作执行入口）、env（默认 process.env）。返回
 * { prepareManagedOpenCodeEnv, registerRoutes, execute }。
 */
export const createAgentToolRuntime = (dependencies) => {
  const {
    crypto,
    fsPromises,
    path,
    dataDir,
    getActivePort,
    executeAction,
    env = process.env,
  } = dependencies;
  // 插件源码的落盘目录与文件路径（dataDir/agent-tool/ompchamber-plugin.js）。
  const pluginDirectory = path.join(dataDir, 'agent-tool');
  const pluginPath = path.join(pluginDirectory, 'ompchamber-plugin.js');
  // 当前子进程的 bearer token；每次 prepareManagedOpenCodeEnv 重新签发。
  let activeToken = null;

  /**
   * 为即将启动的 OpenCode 子进程准备环境：写出插件文件（0600 权限）、
   * 签发新 token、把插件注入 OPENCODE_CONFIG_CONTENT，返回带
   * OMPCHAMBER_AGENT_TOOL_URL 与 OMPCHAMBER_AGENT_TOOL_TOKEN 的环境变量
   * 集。端口不可用、或三个工具全被关闭时抛错。
   */
  const prepareManagedOpenCodeEnv = async ({ includeControl = true, includeWeb = true, includeMemory = true } = {}) => {
    const port = getActivePort();
    if (!Number.isInteger(port) || port <= 0) {
      throw new Error('OMPChamber listener port is unavailable for managed tool injection');
    }
    if (!includeControl && !includeWeb && !includeMemory) {
      throw new Error('At least one OMPChamber managed tool must be enabled to inject the plugin');
    }
    await fsPromises.mkdir(pluginDirectory, { recursive: true });
    await fsPromises.writeFile(pluginPath, createPluginSource({ includeControl, includeWeb, includeMemory }), { mode: 0o600 });
    activeToken = crypto.randomBytes(32).toString('base64url');
    const pluginUrl = pathToFileURL(pluginPath).href;
    return {
      OPENCODE_CONFIG_CONTENT: mergePluginConfig(env.OPENCODE_CONFIG_CONTENT, pluginUrl),
      OMPCHAMBER_AGENT_TOOL_URL: `http://127.0.0.1:${port}/api/ompchamber/agent-tool`,
      OMPCHAMBER_AGENT_TOOL_TOKEN: activeToken,
    };
  };

  /**
   * 回调路由鉴权：必须已签发 token、请求来自 loopback、且 Authorization
   * bearer 与 token 长度相等并经 timingSafeEqual 比对通过；任何一环不
   * 满足都拒绝。
   */
  const authorize = (req) => {
    if (!activeToken || !isLoopbackAddress(req.socket?.remoteAddress)) return false;
    const header = asNonEmptyString(req.headers?.authorization);
    if (!header?.startsWith('Bearer ')) return false;
    const provided = Buffer.from(header.slice(7));
    const expected = Buffer.from(activeToken);
    return provided.length === expected.length && crypto.timingSafeEqual(provided, expected);
  };

  /**
   * 执行一次工具调用：先把模型丢掉命名空间的动作名解析回调用工具自己
   * 的动作集，再校验白名单 ACTIONS，最后交给 executeAction。所有失败都
   * 以结构化结果（而非异常）返回，4xx 归类为 usage，其余为 runtime。
   */
  const execute = async (payload = {}, options = {}) => {
    const requested = asNonEmptyString(payload.input?.action);
    // Resolved against the calling tool's own actions: models drop the
    // namespace that the tool's name already implies, and answering "read" with
    // a bare "unsupported" leaves them to guess a second wrong name.
    const resolution = resolveAgentToolAction(requested, asNonEmptyString(payload.tool));
    if (resolution.error) {
      return createResult({ ok: false, action: requested, error: { message: resolution.error, kind: 'usage' } });
    }
    const action = resolution.action;
    if (!ACTIONS.has(action)) {
      return createResult({ ok: false, action, error: { message: `Unsupported OMPChamber action: ${action}`, kind: 'usage' } });
    }
    if (typeof executeAction !== 'function') {
      return createResult({ ok: false, action, error: { message: 'OMPChamber control service is unavailable', kind: 'runtime' } });
    }
    try {
      const data = await executeAction(action, { ...payload.input, action }, payload.contextDirectory, options);
      return createResult({ ok: true, action, data });
    } catch (error) {
      return createResult({
        ok: false,
        action,
        ...(error?.partial === true ? { data: {
          partial: true,
          partialAction: error.partialAction,
          sessionId: error.sessionId,
          directory: error.directory,
        } } : {}),
        error: {
          message: error instanceof Error ? error.message : String(error),
          kind: Number(error?.statusCode) >= 400 && Number(error?.statusCode) < 499 ? 'usage' : 'runtime',
        },
      });
    }
  };

  /**
   * 在 express 应用上挂载 /api/ompchamber/agent-tool 回调路由：先鉴权，
   * 再把 body 交给 execute；客户端断开时经 AbortController 取消在途动
   * 作，异常也包装成结构化结果返回。
   */
  const registerRoutes = (app, express) => {
    app.post('/api/ompchamber/agent-tool', express.json({ limit: '1mb' }), async (req, res) => {
      if (!authorize(req)) return res.status(401).json({ error: 'Unauthorized' });
      const controller = new AbortController();
      // 连接断开（请求中断或响应关闭）时转发中止信号、取消在途动作。
      const abortOnDisconnect = () => {
        if (!res.writableEnded) controller.abort();
      };
      req.once('aborted', abortOnDisconnect);
      res.once('close', abortOnDisconnect);
      try {
        return res.json(await execute(req.body, { signal: controller.signal }));
      } catch (error) {
        return res.json(createResult({
          ok: false,
          action: req.body?.input?.action,
          error: { message: error instanceof Error ? error.message : String(error), kind: 'runtime' },
        }));
      } finally {
        req.off('aborted', abortOnDisconnect);
        res.off('close', abortOnDisconnect);
      }
    });
  };

  return {
    prepareManagedOpenCodeEnv,
    registerRoutes,
    execute,
  };
};
