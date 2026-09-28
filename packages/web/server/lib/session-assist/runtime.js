// Session assist: after a session goes idle and stays quiet, generate a short
// recap of the agent's last reply plus one suggested user follow-up with the
// small model, and store both on the session's metadata
// (metadata.ompchamber.assist). Clients decide visibility from
// assist.forMessageID — a new message makes the payload stale everywhere
// without any extra writes.
//
// Purely event-driven: only sessions that transition busy→idle while the
// server is running ever generate anything. No backfill, no session scans.

/**
 * 中文说明：会话助手（session assist）运行时。会话空闲并静置一段时间后，
 * 用小模型生成“最后一条助手回复的简短回顾 + 一条可直接发送的后续建议”，
 * 两者写入会话 metadata（metadata.ompchamber.assist）。客户端以
 * assist.forMessageID 判断新鲜度——新消息一到，所有端的旧载荷自动失效，
 * 无需任何额外写入。
 *
 * 纯事件驱动：只有服务器运行期间发生 busy→idle 迁移的会话才会触发生成；
 * 不回填、不扫描会话。Chat 设置里的两个总开关（sessionRecapEnabled /
 * sessionSuggestionEnabled）默认开启，全关时完全不发起小模型调用与写入。
 */
import fs from 'fs';
import os from 'os';
import path from 'path';

/** OMPChamber 全局设置文件（settings.json）的绝对路径，读取助手开关用。 */
const OMPCHAMBER_SETTINGS_FILE = path.join(
  process.env.OMPCHAMBER_DATA_DIR
    ? path.resolve(process.env.OMPCHAMBER_DATA_DIR)
    : path.join(os.homedir(), '.config', 'ompchamber'),
  'settings.json',
);

// The Chat settings are hard generation switches (default on): when both are
// off, no small-model calls and no metadata writes happen at all. Existing
// payloads stay untouched — clients keep showing them and dismissal still works.
// 中文补充：读取 Chat 设置里的回顾 / 建议两个硬开关（默认全开）；
// 读取或解析失败按全开处理，设置缺失不拖垮后台功能。
const getSessionAssistTargets = () => {
  try {
    const raw = fs.readFileSync(OMPCHAMBER_SETTINGS_FILE, 'utf8');
    const settings = JSON.parse(raw);
    return {
      recap: settings?.sessionRecapEnabled !== false,
      suggestion: settings?.sessionSuggestionEnabled !== false,
    };
  } catch {
    return { recap: true, suggestion: true };
  }
};

/** 空闲后需持续静置多久（60 秒）才触发生成。 */
const IDLE_QUIET_MS = 60_000;
/** 拉取最近消息的条数上限。 */
const TRANSCRIPT_MESSAGE_LIMIT = 12;
/** 单条消息文本的截断上限（字符）。 */
const TRANSCRIPT_PART_CHAR_LIMIT = 6_000;
/** 回顾（recap）文本的截断上限。 */
const RECAP_CHAR_LIMIT = 320;
/** 建议（suggestion）文本的截断上限。 */
const SUGGESTION_CHAR_LIMIT = 500;
/** OpenCode API 请求超时（AbortSignal）。 */
const FETCH_TIMEOUT_MS = 5_000;

/**
 * 组装小模型的 system prompt：要求只回一个 JSON 对象
 * （{ recap?, suggestion? }），按 targets 开关裁剪字段与规则文案
 * （建议必须是一条可点击即发的消息、不给备选项、不重复已知信息等），
 * 空串项过滤后按换行拼接；语言必须跟随对话文本本身。
 */
const buildAssistSystemPrompt = ({ recap, suggestion }) => [
  'You assist a user who chats with a coding agent. Based on the conversation transcript, return exactly one JSON object and nothing else — no prose, no markdown, no code fences.',
  `Shape: {${[recap ? '"recap": string' : '', suggestion ? '"suggestion": string' : ''].filter(Boolean).join(', ')}}`,
  recap
    ? 'recap: at most 20 words. State the substance directly — the facts, result, or conclusion, plus the next move if there is one. NEVER narrate ("The assistant explained…", "The agent did…") — write the content itself, like a note the user jotted down.'
    : '',
  suggestion ? 'suggestion: write ONE immediately sendable next user message addressed TO the coding agent.' : '',
  suggestion ? 'The suggestion should be the most useful next step after the assistant\'s latest reply. It should help the user continue productively, not inspect already-known details.' : '',
  suggestion ? 'Prefer suggestions that ask the agent to make a concrete improvement, implement something specific, validate the latest change, explain tradeoffs, improve the current approach, or continue from the current result.' : '',
  suggestion ? 'Rules for suggestion:' : '',
  suggestion ? '- Output exactly one message the user could click and send without editing.' : '',
  suggestion ? '- Pick one best next action yourself.' : '',
  suggestion ? '- Do not include alternatives, choices, slash-separated options, or "or".' : '',
  suggestion ? '- Do not write "Do X or Y", "Ask whether...", "Maybe...", or "You could...".' : '',
  suggestion ? '- Do not ask for information the assistant already provided.' : '',
  suggestion ? '- Do not ask to see exact code, file paths, prompt locations, or implementation internals unless the assistant did not provide them and they are necessary for the next step.' : '',
  suggestion ? '- Do not produce generic workflow commands like "Run tests" unless testing is clearly the next unresolved step.' : '',
  suggestion ? '- Do not produce meta/debug requests that merely inspect the implementation.' : '',
  suggestion ? '- Use imperative or question form.' : '',
  suggestion ? '- Keep it concise.' : '',
  suggestion ? 'Use these examples to understand how to choose the suggestion. Do not copy their topic or wording unless the current conversation is about the same thing.' : '',
  suggestion ? 'Example 1:' : '',
  suggestion ? 'Assistant reply summary:' : '',
  suggestion ? 'The assistant already identified the file where the feature is implemented, explained what context is sent to the small model, and summarized the current prompt.' : '',
  suggestion ? 'Bad suggestion:' : '',
  suggestion ? '"Show me the exact runtime.js code and where the prompt is built."' : '',
  suggestion ? 'Why bad:' : '',
  suggestion ? 'It asks for information the assistant already provided. It repeats inspection instead of moving to an improvement or decision.' : '',
  suggestion ? 'Good suggestion:' : '',
  suggestion ? '"Suggest how to improve the prompt and context so the generated suggestion is more useful."' : '',
  suggestion ? 'Why good:' : '',
  suggestion ? 'It naturally continues from the analysis and asks for a concrete improvement.' : '',
  suggestion ? 'Example 2:' : '',
  suggestion ? 'Assistant reply summary:' : '',
  suggestion ? 'The assistant implemented a timeline dialog redesign, listed concrete UI changes, and reported that type-check and lint passed.' : '',
  suggestion ? 'Bad suggestion:' : '',
  suggestion ? '"Check whether scrolling or loading older messages works without jumps."' : '',
  suggestion ? 'Why bad:' : '',
  suggestion ? 'It contains an alternative. A suggestion chip must be one sendable message, not a choice the user has to edit.' : '',
  suggestion ? 'Good suggestion:' : '',
  suggestion ? '"Check whether scrolling and loading older messages work without jumps."' : '',
  suggestion ? 'Why good:' : '',
  suggestion ? 'It picks a single validation request that the user can send immediately.' : '',
  'All requested values MUST be written in the same language as the conversation text itself. Ignore any other language preferences or personalization you may have — only the conversation text decides the language.',
  'Use double quotes for JSON strings, no trailing commas.',
].filter(Boolean).join('\n');

/** 从模型输出（可能裹在 markdown 代码块或散文里）提取首个可解析的 JSON 对象；找不到返回 null。 */
const extractJsonObject = (value) => {
  const text = String(value ?? '').trim();
  const fenced = text.match(/```(?:json)?\s*([\s\S]*?)```/i);
  const candidate = (fenced?.[1] ?? text).trim();
  const start = candidate.indexOf('{');
  if (start < 0) return null;
  for (let end = candidate.length; end > start; end -= 1) {
    if (candidate[end - 1] !== '}') continue;
    try {
      const parsed = JSON.parse(candidate.slice(start, end));
      if (parsed && typeof parsed === 'object' && !Array.isArray(parsed)) {
        return parsed;
      }
    } catch {
      // keep scanning — models wrap JSON in prose sometimes
    }
  }
  return null;
};

/** 识别 session.status 事件并提取 { sessionId, type, directory }；非该类型或缺关键字段返回 null。 */
const extractSessionStatus = (payload) => {
  if (!payload || payload.type !== 'session.status') return null;
  const properties = payload.properties && typeof payload.properties === 'object' ? payload.properties : {};
  const status = properties.status && typeof properties.status === 'object' ? properties.status : {};
  const info = properties.info && typeof properties.info === 'object' ? properties.info : {};
  const sessionId = typeof properties.sessionID === 'string' ? properties.sessionID.trim() : '';
  const type = typeof status.type === 'string'
    ? status.type.trim()
    : (typeof info.type === 'string' ? info.type.trim() : '');
  if (!sessionId || !type) return null;
  const directory = typeof properties.directory === 'string' && properties.directory
    ? properties.directory
    : (typeof info.directory === 'string' ? info.directory : '');
  return { sessionId, type, directory };
};

/** 识别 message.updated 里的用户消息，返回 { sessionId, createdAt }；非用户消息或缺 sessionID 返回 null。 */
const extractUserMessage = (payload) => {
  if (!payload || payload.type !== 'message.updated') return null;
  const info = payload.properties?.info;
  if (!info || typeof info !== 'object' || info.role !== 'user') return null;
  if (typeof info.sessionID !== 'string' || !info.sessionID) return null;
  return {
    sessionId: info.sessionID,
    createdAt: typeof info.time?.created === 'number' ? info.time.created : 0,
  };
};

/** 把消息 parts 中全部文本块拼接为纯文本，并按 TRANSCRIPT_PART_CHAR_LIMIT 截断。 */
const messagePartsToText = (message) => {
  const parts = Array.isArray(message?.parts) ? message.parts : [];
  return parts
    .map((part) => (part?.type === 'text' && typeof part.text === 'string' ? part.text : ''))
    .filter(Boolean)
    .join('\n')
    .slice(0, TRANSCRIPT_PART_CHAR_LIMIT);
};

/**
 * 创建会话助手运行时。
 *
 * @param {object} options 注入依赖
 * @param {Function} options.buildOpenCodeUrl 构造 OpenCode API URL
 * @param {Function} options.getOpenCodeAuthHeaders 取 OpenCode 认证头
 * @param {Function} options.getSmallModelService 惰性获取小模型服务
 * @param {number} [options.quietMs] 静置窗口毫秒数（默认 IDLE_QUIET_MS，测试可调小）
 * @returns {object} { processPayload, stop }：前者消费 OpenCode 事件流，
 *   后者停机并清理全部定时器
 */
export const createSessionAssistRuntime = ({
  buildOpenCodeUrl,
  getOpenCodeAuthHeaders,
  getSmallModelService,
  quietMs = IDLE_QUIET_MS,
}) => {
  /** sessionId 到已武装定时器的映射（含武装时间 armedAt，供“新消息取消”判断）。 */
  const timers = new Map();
  /** 正在生成中的 sessionId 集合，防止同会话重入。 */
  const inflight = new Set();
  /** 停机标记：置位后不再处理任何事件。 */
  let stopped = false;

  /** 取消并移除某会话已武装的定时器（若存在）。 */
  const clearTimer = (sessionId) => {
    const existing = timers.get(sessionId);
    if (existing) {
      clearTimeout(existing.timer);
      timers.delete(sessionId);
    }
  };

  /**
   * 调 OpenCode API 的通用 fetch：拼接 directory 查询参数、携带认证头与
   * 超时；非 2xx 抛错，响应体 JSON 解析失败返回 null。
   */
  const openCodeFetch = async (path, { directory, method = 'GET', body } = {}) => {
    const base = buildOpenCodeUrl(path, '');
    const url = directory ? `${base}?directory=${encodeURIComponent(directory)}` : base;
    const response = await fetch(url, {
      method,
      headers: {
        Accept: 'application/json',
        ...(body ? { 'Content-Type': 'application/json' } : {}),
        ...getOpenCodeAuthHeaders(),
      },
      ...(body ? { body: JSON.stringify(body) } : {}),
      signal: AbortSignal.timeout(FETCH_TIMEOUT_MS),
    });
    if (!response.ok) {
      throw new Error(`OpenCode ${method} ${path} failed with ${response.status}`);
    }
    return response.json().catch(() => null);
  };

  /** 拉取会话最近 N 条消息；请求失败、非 2xx 或结构异常一律返回 null。 */
  const fetchRecentMessages = async (sessionId, directory) => {
    const base = buildOpenCodeUrl(`/session/${encodeURIComponent(sessionId)}/message`, '');
    const params = new URLSearchParams({ limit: String(TRANSCRIPT_MESSAGE_LIMIT) });
    if (directory) params.set('directory', directory);
    const response = await fetch(`${base}?${params.toString()}`, {
      method: 'GET',
      headers: { Accept: 'application/json', ...getOpenCodeAuthHeaders() },
      signal: AbortSignal.timeout(FETCH_TIMEOUT_MS),
    });
    if (!response.ok) return null;
    const messages = await response.json().catch(() => null);
    return Array.isArray(messages) ? messages : null;
  };

  /**
   * 为会话生成一次助手载荷：读取开关并短路（全关直接返回）；跳过子会话；
   * 取最近一轮对话（用户消息 + 助手回复，经 assistant.parentID 关联），
   * 调小模型产出 JSON（限制在与会话相同的 provider 下）；对结果做长度
   * 截断与“语言幻觉”防线（输出出现的字符集必须在对话里出现过，按字段
   * 独立丢弃）；确认会话尾部未移动后，从最新读取的 metadata 合并写入
   * metadata.ompchamber.assist（含 forMessageID / generatedAt）。任何一步
   * 不满足都静默放弃——后台功能，绝不重试、不刷错误日志。
   */
  const generateAssist = async (sessionId, directory) => {
    const targets = getSessionAssistTargets();
    if (!targets.recap && !targets.suggestion) return;
    const session = await openCodeFetch(`/session/${encodeURIComponent(sessionId)}`, { directory })
      .catch((error) => {
        console.warn(`[session-assist] session fetch failed: ${error?.message || error}`);
        return null;
      });
    if (!session || typeof session !== 'object') return;
    // Sub-agent/task sessions never surface in chat — skip them.
    if (typeof session.parentID === 'string' && session.parentID) return;

    const messages = await fetchRecentMessages(sessionId, directory);
    if (!messages || messages.length === 0) {
      console.warn('[session-assist] no messages fetched');
      return;
    }

    let lastAssistant = null;
    for (let i = messages.length - 1; i >= 0; i -= 1) {
      const info = messages[i]?.info;
      if (info?.role === 'assistant') {
        lastAssistant = messages[i];
        break;
      }
    }
    const lastAssistantInfo = lastAssistant?.info;
    if (!lastAssistantInfo?.id) return;

    // Only the last exchange: the assistant reply plus the user message it
    // answered (assistant info.parentID → user info.id). Everything else is
    // token waste for a one-line recap and a single suggestion.
    const parentUserMessage = typeof lastAssistantInfo.parentID === 'string' && lastAssistantInfo.parentID
      ? messages.find((message) => message?.info?.id === lastAssistantInfo.parentID && message?.info?.role === 'user')
      : null;
    const userText = parentUserMessage ? messagePartsToText(parentUserMessage) : '';
    const assistantText = messagePartsToText(lastAssistant);
    const transcript = [
      userText ? `User:\n${userText}` : '',
      assistantText ? `Assistant:\n${assistantText}` : '',
    ].filter(Boolean).join('\n\n');
    if (!transcript) return;

    const { generateSmallModelText } = await getSmallModelService();
    const requestedFields = [targets.recap ? 'recap' : '', targets.suggestion ? 'suggestion' : '']
      .filter(Boolean)
      .join(' and ');
    // Instruct the language by example, not by description — account-side
    // personalization (e.g. the ChatGPT backend knowing the user's locale)
    // otherwise leaks a different language into the output.
    const languageSample = (userText || assistantText).slice(0, 200).replace(/\s+/g, ' ').trim();
    let generated;
    try {
      generated = await generateSmallModelText({
        // Background feature: conversation content must never leave the
        // session's own provider unless the user explicitly picked a small
        // model (settings override / opencode config).
        restrictToPreferredProvider: true,
        prompt: `The latest exchange in the conversation:\n\n${transcript}\n\nWrite ${requestedFields} in the SAME language as this sample from the conversation: "${languageSample}"`,
        system: buildAssistSystemPrompt(targets),
        directory,
        preferredProviderID: typeof lastAssistantInfo.providerID === 'string' ? lastAssistantInfo.providerID : undefined,
        preferredModelID: typeof lastAssistantInfo.modelID === 'string' ? lastAssistantInfo.modelID : undefined,
      });
    } catch (error) {
      // No authenticated provider (404) or a transient model failure — this is
      // background sugar, never retry loops or logs spam.
      if (Number(error?.statusCode) !== 404) {
        console.warn('[session-assist] generation failed:', error?.message || error);
      }
      return;
    }

    const structured = extractJsonObject(generated?.text);
    let recap = targets.recap && typeof structured?.recap === 'string' ? structured.recap.trim().slice(0, RECAP_CHAR_LIMIT) : '';
    let suggestion = targets.suggestion && typeof structured?.suggestion === 'string' ? structured.suggestion.trim().slice(0, SUGGESTION_CHAR_LIMIT) : '';

    // Hard guard against language hallucination: if the conversation contains
    // no Cyrillic/CJK at all, the output must not either (and drop per-field,
    // so one hallucinated field doesn't kill the other).
    const hasCyrillic = (text) => /[\u0400-\u04FF]/.test(text);
    const hasCjk = (text) => /[\u3040-\u30FF\u4E00-\u9FFF\uAC00-\uD7AF]/.test(text);
    const inputText = `${userText}\n${assistantText}`;
    const scriptMismatch = (text) => (hasCyrillic(text) && !hasCyrillic(inputText))
      || (hasCjk(text) && !hasCjk(inputText));
    if (recap && scriptMismatch(recap)) {
      console.warn('[session-assist] dropped recap: language mismatch with conversation');
      recap = '';
    }
    if (suggestion && scriptMismatch(suggestion)) {
      console.warn('[session-assist] dropped suggestion: language mismatch with conversation');
      suggestion = '';
    }
    if (!recap && !suggestion) return;

    // The session may have moved on while we generated — a stale patch would
    // flash outdated content, so re-check the tail before writing.
    const latest = await fetchRecentMessages(sessionId, directory);
    const latestAssistantId = (() => {
      if (!latest) return null;
      for (let i = latest.length - 1; i >= 0; i -= 1) {
        const info = latest[i]?.info;
        if (info?.role === 'assistant') return info.id;
        if (info?.role === 'user') return null;
      }
      return null;
    })();
    if (latestAssistantId !== lastAssistantInfo.id) {
      console.log('[session-assist] tail moved on, dropping result');
      return;
    }

    // Merge from a FRESH read: generation takes tens of seconds, and merging
    // from the session snapshot fetched before it would clobber any metadata
    // written meanwhile (suggestion dismissals, review links, …).
    const freshSession = await openCodeFetch(`/session/${encodeURIComponent(sessionId)}`, { directory })
      .catch(() => null);
    const currentMetadata = freshSession?.metadata && typeof freshSession.metadata === 'object'
      ? freshSession.metadata
      : (session.metadata && typeof session.metadata === 'object' ? session.metadata : {});
    const currentNamespace = currentMetadata.ompchamber && typeof currentMetadata.ompchamber === 'object'
      ? currentMetadata.ompchamber
      : {};

    console.log(`[session-assist] generated for ${sessionId} via ${generated.providerID}/${generated.modelID}`);
    await openCodeFetch(`/session/${encodeURIComponent(sessionId)}`, {
      directory,
      method: 'PATCH',
      body: {
        metadata: {
          ...currentMetadata,
          ompchamber: {
            ...currentNamespace,
            assist: {
              recap,
              suggestion,
              forMessageID: lastAssistantInfo.id,
              generatedAt: Date.now(),
            },
          },
        },
      },
    });
  };

  /** （重新）武装某会话的静置定时器：到点且未停机、未在途时触发 generateAssist，并登记 inflight 防重入；定时器 unref 不阻塞进程退出。 */
  const armTimer = (sessionId, directory) => {
    clearTimer(sessionId);
    const timer = setTimeout(() => {
      timers.delete(sessionId);
      if (stopped || inflight.has(sessionId)) return;
      inflight.add(sessionId);
      generateAssist(sessionId, directory)
        .catch((error) => {
          console.warn('[session-assist] failed:', error?.message || error);
        })
        .finally(() => {
          inflight.delete(sessionId);
        });
    }, quietMs);
    if (typeof timer?.unref === 'function') timer.unref();
    timers.set(sessionId, { timer, armedAt: Date.now() });
  };

  /**
   * 消费一条 OpenCode 事件：session.status 为 idle 时武装定时器，其他
   * 状态取消定时器；message.updated 的用户消息在“创建时间不早于武装
   * 时间”时取消定时器（OpenCode 会对历史消息重发事件，只有新消息才
   * 代表用户真的继续了对话）。
   */
  const processPayload = (payload, directoryHint = '') => {
    if (stopped) return;
    const status = extractSessionStatus(payload);
    if (status) {
      if (status.type === 'idle') {
        armTimer(status.sessionId, status.directory || directoryHint);
      } else {
        clearTimer(status.sessionId);
      }
      return;
    }
    const userMessage = extractUserMessage(payload);
    if (userMessage) {
      // OpenCode re-emits message.updated for OLD user messages after the
      // session settles (post-completion metadata patches). Only a message
      // created after the timer was armed means the user actually moved on.
      const armed = timers.get(userMessage.sessionId);
      if (armed && userMessage.createdAt >= armed.armedAt) {
        clearTimer(userMessage.sessionId);
      }
    }
  };

  /** 停机：置位 stopped 并清掉全部已武装定时器。 */
  const stop = () => {
    stopped = true;
    for (const { timer } of timers.values()) {
      clearTimeout(timer);
    }
    timers.clear();
  };

  return { processPayload, stop };
};
