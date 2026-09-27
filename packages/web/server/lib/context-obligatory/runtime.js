/**
 * "必留上下文"（context obligatory）运行时。
 *
 * 监听 OpenCode 会话压缩（session.compacted）事件：压缩会把用户显式固定
 * 的消息和项目知识一起裁掉，此模块在压缩后立即把两者合成一条 synthetic
 * 消息重新注入会话，并把压缩游标（及知识签名）写回会话 metadata，
 * 保证同一轮压缩只注入一次。错误只告警，绝不让注入失败影响主流程。
 */
/** 单次 OpenCode API 请求的超时。 */
const FETCH_TIMEOUT_MS = 15_000;
/** 注入前拉取最近消息（定位压缩摘要与执行模型）的条数上限。 */
const MESSAGE_FETCH_LIMIT = 20;

/** 值是否为非数组普通对象（防御性读取外部 JSON 结构用）。 */
const isRecord = (value) => Boolean(value && typeof value === 'object' && !Array.isArray(value));

/** 从会话对象里安全读出 metadata.ompchamber 与合法的固定消息列表（id/createdAt/role 齐全的项）。 */
const readContextState = (session) => {
  const metadata = isRecord(session?.metadata) ? session.metadata : {};
  const ompchamber = isRecord(metadata.ompchamber) ? metadata.ompchamber : {};
  const messages = Array.isArray(ompchamber.context_obligatory_messages)
    ? ompchamber.context_obligatory_messages.filter((item) =>
      isRecord(item)
      && typeof item.id === 'string'
      && typeof item.createdAt === 'number'
      && (item.role === 'user' || item.role === 'assistant'))
    : [];
  return { metadata, ompchamber, messages };
};

/** 把固定消息按时间线拼成注入提示词：头部说明使用规则（静默续用、仅无任务时简短总结），正文按时间戳排序。 */
const buildContextPrompt = (entries) => {
  const timeline = entries.map(({ pinned, text }) => {
    const timestamp = new Date(pinned.createdAt).toISOString();
    return `## ${pinned.role} — ${timestamp}\n\n${text}`;
  }).join('\n\n---\n\n');
  return [
    'The following messages are from the compacted conversation. The user explicitly marked them as important and required in your context. Pay close attention to them; they may have been sent by either the user or you before compaction.',
    'Use them while continuing the pre-compaction work. Do not treat this context restoration as a new standalone task.',
    'If any tasks or next steps remain, do not acknowledge, summarize, or mention this restored context in a separate response. Simply continue the work and use it silently as background context. Do not append a recap of it after completing those tasks. Only if no tasks or next steps remain, give the user a very brief summary of the important restored context in no more than one short paragraph, without lists or a detailed recap.',
    '',
    timeline,
  ].join('\n');
};

/**
 * 创建必留上下文运行时。
 *
 * @param {(path: string, directory: string) => string} buildOpenCodeUrl 构造 OpenCode API 完整 URL
 * @param {() => object} getOpenCodeAuthHeaders 每次请求附带的认证头
 * @param {object} [sessionKnowledgeRuntime] 项目知识运行时（resolvePending/readPins/metadataKey），缺省则只恢复固定消息
 * @returns {{ processPayload: Function, stop: Function }} SSE 事件入口与停止函数
 */
export const createContextObligatoryRuntime = ({
  buildOpenCodeUrl,
  getOpenCodeAuthHeaders,
  sessionKnowledgeRuntime = null,
}) => {
  // 进行中的注入按会话 id 去重。
  const inflight = new Set();
  // stop() 之后忽略一切事件。
  let stopped = false;

  /**
   * 调 OpenCode HTTP API：拼 URL 与 directory 查询参数，附带认证头，
   * 15 秒超时；非 2xx 抛错，空/坏 JSON 容错为 null。
   */
  const openCodeFetch = async (fetchPath, { directory, method = 'GET', body, query } = {}) => {
    const params = new URLSearchParams(query || {});
    if (directory) params.set('directory', directory);
    const search = params.toString();
    const response = await fetch(`${buildOpenCodeUrl(fetchPath, '')}${search ? `?${search}` : ''}`, {
      method,
      headers: {
        Accept: 'application/json',
        ...(body ? { 'Content-Type': 'application/json' } : {}),
        ...getOpenCodeAuthHeaders(),
      },
      ...(body ? { body: JSON.stringify(body) } : {}),
      signal: AbortSignal.timeout(FETCH_TIMEOUT_MS),
    });
    if (!response.ok) throw new Error(`OpenCode ${method} ${fetchPath} failed with ${response.status}`);
    return response.json().catch(() => null);
  };

  /**
   * 对一个刚压缩完的会话执行一次注入：读会话固定消息与项目知识，
   * 从最近消息里定位压缩摘要与上一个执行模型/agent，把两部分合成一条
   * synthetic 消息发回会话，然后把压缩游标与知识签名 PATCH 进 metadata。
   * 无固定消息且无知识、找不到摘要、或游标表明已注入过时直接返回。
   */
  const tick = async (sessionId, directory) => {
    const session = await openCodeFetch(`/session/${encodeURIComponent(sessionId)}`, { directory });
    if (session?.parentID) return;
    const state = readContextState(session);

    /**
     * Project knowledge rides along with the pinned messages. Compaction takes
     * both away, and both are restored for the same reason, so they travel as
     * one message: two synthetic turns back to back would read as the agent
     * being interrupted twice.
     */
    const knowledge = sessionKnowledgeRuntime
      ? await sessionKnowledgeRuntime
        .resolvePending(
          directory,
          // Compaction removed the previously delivered block, so its stored
          // signature is no longer evidence that the session still carries it.
          '',
          sessionKnowledgeRuntime.readPins(session),
        )
        .catch(() => ({ text: '', signature: '' }))
      : { text: '', signature: '' };

    if (state.messages.length === 0 && !knowledge.text) return;

    const recent = await openCodeFetch(`/session/${encodeURIComponent(sessionId)}/message`, {
      directory,
      query: { limit: String(MESSAGE_FETCH_LIMIT) },
    });
    if (!Array.isArray(recent) || recent.length === 0) return;
    const summary = recent.toReversed().find((message) =>
      message?.info?.role === 'assistant' && message.info.summary === true)?.info;
    if (!summary?.id || !summary?.time?.completed) return;
    if (state.ompchamber.context_obligatory_last_compaction_message_id === summary.id) return;

    const fetched = await Promise.allSettled(state.messages.map(async (pinned) => {
      const message = await openCodeFetch(
        `/session/${encodeURIComponent(sessionId)}/message/${encodeURIComponent(pinned.id)}`,
        { directory },
      );
      const text = Array.isArray(message?.parts)
        ? message.parts.filter((part) => part?.type === 'text' && typeof part.text === 'string')
          .map((part) => part.text.trim()).filter(Boolean).join('\n\n')
        : '';
      return { pinned, text };
    }));
    const entries = fetched
      .filter((result) => result.status === 'fulfilled' && result.value.text)
      .map((result) => result.value)
      .sort((left, right) => left.pinned.createdAt - right.pinned.createdAt);
    if (entries.length === 0 && !knowledge.text) return;

    const executionInfo = recent.toReversed().find((message) =>
      message?.info?.role === 'assistant' && message.info.summary !== true)?.info;
    const providerID = typeof executionInfo?.providerID === 'string' ? executionInfo.providerID : '';
    const modelID = typeof executionInfo?.modelID === 'string' ? executionInfo.modelID : '';
    if (!providerID || !modelID) throw new Error('no pre-compaction assistant provider/model');
    const agent = typeof executionInfo.agent === 'string' ? executionInfo.agent : executionInfo.mode;
    await openCodeFetch(`/session/${encodeURIComponent(sessionId)}/prompt_async`, {
      directory,
      method: 'POST',
      body: {
        model: { providerID, modelID },
        ...(typeof agent === 'string' && agent ? { agent } : {}),
        parts: [{
          type: 'text',
          text: [knowledge.text, entries.length > 0 ? buildContextPrompt(entries) : '']
            .filter(Boolean)
            .join('\n\n---\n\n'),
          synthetic: true,
        }],
      },
    });

    const fresh = await openCodeFetch(`/session/${encodeURIComponent(sessionId)}`, { directory });
    const freshState = readContextState(fresh);
    await openCodeFetch(`/session/${encodeURIComponent(sessionId)}`, {
      directory,
      method: 'PATCH',
      body: {
        metadata: {
          ...freshState.metadata,
          ompchamber: {
            ...freshState.ompchamber,
            context_obligatory_last_compaction_message_id: summary.id,
            // Recorded together with the cursor: the session now carries this
            // knowledge again, so the next send must not repeat it.
            ...(knowledge.signature
              ? { [sessionKnowledgeRuntime.metadataKey]: knowledge.signature }
              : {}),
          },
        },
      },
    });
  };

  /**
   * SSE 事件入口：只对 session.compacted 事件触发一次 tick；同一会话并发
   * 去重，stop() 后忽略一切事件，错误只告警不外抛。
   */
  const processPayload = (payload, directoryHint = '') => {
    if (stopped || payload?.type !== 'session.compacted') return;
    const sessionId = payload?.properties?.sessionID;
    if (typeof sessionId !== 'string' || inflight.has(sessionId)) return;
    const directory = payload?.properties?.directory || directoryHint;
    inflight.add(sessionId);
    return tick(sessionId, directory)
      .catch((error) => console.warn('[context-obligatory] injection failed:', error?.message || error))
      .finally(() => inflight.delete(sessionId));
  };

  /** 停止运行时：之后所有事件被忽略（不中断已在途的注入）。 */
  const stop = () => {
    stopped = true;
  };

  return { processPayload, stop };
};
