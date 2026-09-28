/**
 * 会话运行时状态机模块。
 *
 * 跟踪 OpenCode 会话的 busy/idle 活动相位、最近状态与“需要关注”
 * 标记，并把变化以 SSE / 广播事件推给前端。输入是 OpenCode 的
 * session.status SSE 事件，输出包括：活动相位（busy / cooldown /
 * idle，cooldown 是 idle 前的 2 秒缓冲，用于吸收状态抖动）、会话状态
 * 快照、needsAttention（busy 转 idle 且发过用户消息但没有任何客户端
 * 查看时置位，前端据此显示未读），以及 OpenCode 重启后对在跑会话的
 * 中断收尾。状态全部保存在运行时闭包的内存 Map 中，超龄条目由
 * 每小时一次的定时任务回收。
 */
/** 会话 idle 后的 cooldown 缓冲时长（毫秒）：busy 结束先进入 cooldown，到期后才回落 idle。 */
const SESSION_COOLDOWN_DURATION_MS = 2000;
/** 会话状态（sessionStates）条目的最长保留时长：24 小时未更新即被清理。 */
const SESSION_STATE_MAX_AGE_MS = 24 * 60 * 60 * 1000;
/** 关注状态（sessionAttentionStates）条目的最长保留时长：24 小时未变化即被清理。 */
const SESSION_ATTENTION_MAX_AGE_MS = 24 * 60 * 60 * 1000;
/** 活动相位（sessionActivityPhases）条目的最长保留时长：24 小时未更新即被清理。 */
const SESSION_ACTIVITY_MAX_AGE_MS = 24 * 60 * 60 * 1000;
/** 过期状态清理任务的执行间隔（每小时一次）。 */
const SESSION_STATE_CLEANUP_INTERVAL_MS = 60 * 60 * 1000;

/**
 * 从 OpenCode SSE payload 中提取 session.status 更新：仅接受
 * type === 'session.status' 的事件，规范化出 sessionId / type /
 * eventId 及可选的 attempt / message / next（取值以 properties.status
 * 对象优先，兼容旧版 properties.info 对象，见函数内英文注释）。
 * sessionId 或 type 缺失时返回 null（即非状态事件，调用方直接忽略）。
 */
const extractSessionStatusUpdate = (payload) => {
  if (!payload || payload.type !== 'session.status') {
    return null;
  }

  const properties = payload.properties && typeof payload.properties === 'object' ? payload.properties : {};
  const status = properties.status && typeof properties.status === 'object' ? properties.status : {};
  const info = properties.info && typeof properties.info === 'object' ? properties.info : {};
  const sessionId = typeof properties.sessionID === 'string' ? properties.sessionID.trim() : '';
  // Canonical OpenCode schema uses properties.status.type. Keep legacy info.type fallback for compatibility.
  const type = typeof status.type === 'string'
    ? status.type.trim()
    : (typeof info.type === 'string' ? info.type.trim() : '');

  if (!sessionId || !type) {
    return null;
  }

  return {
    sessionId,
    type,
    eventId: typeof payload.id === 'string' ? payload.id : '',
    attempt: typeof status.attempt === 'number'
      ? status.attempt
      : (typeof info.attempt === 'number' ? info.attempt : undefined),
    message: typeof status.message === 'string'
      ? status.message
      : (typeof info.message === 'string' ? info.message : undefined),
    next: typeof status.next === 'number'
      ? status.next
      : (typeof info.next === 'number' ? info.next : undefined),
  };
};

/**
 * 创建会话运行时实例（全部状态封闭在返回对象的闭包内，进程内唯一）。
 *
 * @param writeSseEvent 向单个 SSE 客户端写事件的函数（无广播通道时
 *   的降级路径，单客户端失败会被吞掉）
 * @param getNotificationClients 返回当前 SSE 客户端集合的函数
 * @param broadcastEvent 广播函数（存在时优先于逐客户端 SSE 写）
 * @returns 会话运行时 API：processOpenCodeSsePayload、各类快照与
 *   单会话查询、已读/用户消息标记、重启中断收尾与 dispose。
 */
export const createSessionRuntime = ({ writeSseEvent, getNotificationClients, broadcastEvent }) => {
  // 会话活动相位表：sessionId -> { phase: busy|cooldown|idle, updatedAt }。
  const sessionActivityPhases = new Map();
  // cooldown 定时器表：sessionId -> 到期后把 cooldown 回落为 idle 的 setTimeout。
  const sessionActivityCooldowns = new Map();
  // 会话状态表：sessionId -> { status, lastUpdateAt, lastEventId, metadata }。
  const sessionStates = new Map();
  // 关注状态表：sessionId -> { needsAttention, lastUserMessageAt,
  // lastStatusChangeAt, viewedByClients, status }。
  const sessionAttentionStates = new Map();
  // busy 相位会话计数（getActiveSessionCount 的数据源）。
  let activeSessionCount = 0;

  // 取出（或懒创建）会话的关注状态；sessionId 非法返回 null。新状态
  // 默认 idle、needsAttention 为 false、viewedByClients 为空集合。
  const getOrCreateAttentionState = (sessionId) => {
    if (!sessionId || typeof sessionId !== 'string') return null;

    let state = sessionAttentionStates.get(sessionId);
    if (!state) {
      state = {
        needsAttention: false,
        lastUserMessageAt: null,
        lastStatusChangeAt: Date.now(),
        viewedByClients: new Set(),
        status: 'idle',
      };
      sessionAttentionStates.set(sessionId, state);
    }
    return state;
  };

  // 设置会话活动相位并处理迁移细节：同相位直接返回 false；cooldown
  // 只允许从 busy 进入；迁移时清掉旧 cooldown 定时器并同步增减
  // activeSessionCount；进入 cooldown 时安排 2 秒后回落 idle 的定时器
  // （期间再次 busy 会取消它）；每次成功迁移都广播
  // ompchamber:session-activity 事件。返回是否真的发生了迁移。
  const setSessionActivityPhase = (sessionId, phase) => {
    if (!sessionId || typeof sessionId !== 'string') return false;

    const current = sessionActivityPhases.get(sessionId);
    if (current?.phase === phase) return false;
    if (phase === 'cooldown' && current?.phase !== 'busy') {
      return false;
    }

    const existingTimer = sessionActivityCooldowns.get(sessionId);
    if (existingTimer) {
      clearTimeout(existingTimer);
      sessionActivityCooldowns.delete(sessionId);
    }

    const wasActive = current?.phase === 'busy';
    const isActive = phase === 'busy';
    if (wasActive !== isActive) {
      activeSessionCount = Math.max(0, activeSessionCount + (isActive ? 1 : -1));
    }
    sessionActivityPhases.set(sessionId, { phase, updatedAt: Date.now() });

    if (phase === 'cooldown') {
      const timer = setTimeout(() => {
        const now = sessionActivityPhases.get(sessionId);
        if (now?.phase === 'cooldown') {
          setSessionActivityPhase(sessionId, 'idle');
          return;
        }
        sessionActivityCooldowns.delete(sessionId);
      }, SESSION_COOLDOWN_DURATION_MS);
      sessionActivityCooldowns.set(sessionId, timer);
    }

    if (typeof broadcastEvent === 'function') {
      broadcastEvent({
        type: 'ompchamber:session-activity',
        properties: {
          sessionId,
          phase,
        },
      });
    }

    return true;
  };

  // 更新关注状态中的 status 并刷新 lastStatusChangeAt；从 busy/retry
  // 转为 idle 且此前发过用户消息、尚无任何客户端查看时置
  // needsAttention = true（前端未读提示的判定来源）。
  const updateSessionAttentionStatus = (sessionId, status) => {
    const state = getOrCreateAttentionState(sessionId);
    if (!state) return;

    const prevStatus = state.status;
    state.status = status;
    state.lastStatusChangeAt = Date.now();

    if ((prevStatus === 'busy' || prevStatus === 'retry') && status === 'idle') {
      if (state.lastUserMessageAt && state.viewedByClients.size === 0) {
        state.needsAttention = true;
      }
    }
  };

  // 会话状态更新入口：5 秒内同状态重复事件直接忽略（OpenCode 重启
  // 中断场景例外，强制落状态）；写入 sessionStates（metadata 与旧值
  // 浅合并）后同步关注状态；首次记录、状态真正变化、needsAttention
  // 翻转或重启中断时，广播合成的 ompchamber:session-status 事件
  // （有 broadcastEvent 用之，否则逐个 SSE 客户端写、单客户端失败
  // 静默）；最后联动活动相位：busy/retry 映射 busy、其余映射 idle，
  // 但 cooldown 相位中的 idle 事件不覆盖 cooldown。
  const updateSessionState = (sessionId, status, eventId, metadata = {}) => {
    if (!sessionId || typeof sessionId !== 'string') return;

    const now = Date.now();
    const existing = sessionStates.get(sessionId);
    const existingAttentionState = sessionAttentionStates.get(sessionId);
    const isRestartInterruption = metadata.reason === 'opencode-restart';
    if (existing && existing.lastUpdateAt > now - 5000 && status === existing.status && !isRestartInterruption) {
      return;
    }

    sessionStates.set(sessionId, {
      status,
      lastUpdateAt: now,
      lastEventId: eventId || `server-${now}`,
      metadata: { ...existing?.metadata, ...metadata },
    });

    updateSessionAttentionStatus(sessionId, status);
    const attentionState = sessionAttentionStates.get(sessionId);
    const attentionChanged = !!attentionState && existingAttentionState?.needsAttention !== attentionState.needsAttention;
    const clients = getNotificationClients();
    if (!existing || existing.status !== status || attentionChanged || isRestartInterruption) {
      const state = sessionStates.get(sessionId);
      const syntheticPayload = {
        type: 'ompchamber:session-status',
        properties: {
          sessionID: sessionId,
          status: state.status,
          timestamp: state.lastUpdateAt,
          metadata: state.metadata,
          needsAttention: attentionState?.needsAttention ?? false,
        },
      };

      if (typeof broadcastEvent === 'function') {
        broadcastEvent(syntheticPayload);
      } else if (clients.size > 0) {
        for (const res of clients) {
          try {
            writeSseEvent(res, syntheticPayload);
          } catch {
          }
        }
      }
    }

    const phase = status === 'busy' || status === 'retry' ? 'busy' : 'idle';
    if (phase !== 'idle' || sessionActivityPhases.get(sessionId)?.phase !== 'cooldown') {
      setSessionActivityPhase(sessionId, phase);
    }
  };

  // 全量会话状态快照（跳过超过 24 小时未更新的条目），形如
  // { sessionId: { status, lastUpdateAt, metadata } }。
  const getSessionStateSnapshot = () => {
    const result = {};
    const now = Date.now();
    for (const [sessionId, data] of sessionStates) {
      if (now - data.lastUpdateAt > SESSION_STATE_MAX_AGE_MS) continue;
      result[sessionId] = {
        status: data.status,
        lastUpdateAt: data.lastUpdateAt,
        metadata: data.metadata,
      };
    }
    return result;
  };

  // 查询单个会话的原始状态记录（含 lastEventId），无记录返回 null。
  const getSessionState = (sessionId) => {
    if (!sessionId) return null;
    return sessionStates.get(sessionId) || null;
  };

  // 标记某客户端正在查看会话：记入 viewedByClients；若此前
  // needsAttention 为 true 则清零，并广播 needsAttention = false 的
  // 合成 session-status 事件，让其它客户端同步消掉未读标记。
  const markSessionViewed = (sessionId, clientId) => {
    const state = getOrCreateAttentionState(sessionId);
    if (!state) return;

    const wasNeedsAttention = state.needsAttention;
    state.viewedByClients.add(clientId);

    if (wasNeedsAttention) {
      state.needsAttention = false;

      const syntheticPayload = {
        type: 'ompchamber:session-status',
        properties: {
          sessionID: sessionId,
          status: state.status,
          timestamp: Date.now(),
          metadata: {},
          needsAttention: false,
        },
      };

      if (typeof broadcastEvent === 'function') {
        broadcastEvent(syntheticPayload);
      } else {
        const clients = getNotificationClients();
        for (const res of clients) {
          try {
            writeSseEvent(res, syntheticPayload);
          } catch {
          }
        }
      }
    }
  };

  // 移除某客户端的查看标记（不触发事件）；会话无关注状态时忽略。
  const markSessionUnviewed = (sessionId, clientId) => {
    const state = sessionAttentionStates.get(sessionId);
    if (!state) return;
    state.viewedByClients.delete(clientId);
  };

  // 记录用户在该会话发送消息的时间：needsAttention 判定要用——
  // busy 转 idle 时只对发过消息且无人查看的会话置位。
  const markUserMessageSent = (sessionId) => {
    const state = getOrCreateAttentionState(sessionId);
    if (!state) return;
    state.lastUserMessageAt = Date.now();
  };

  // 全量关注状态快照（跳过超过 24 小时未变化的条目），形如
  // { sessionId: { needsAttention, lastUserMessageAt,
  // lastStatusChangeAt, status, isViewed } }。
  const getSessionAttentionSnapshot = () => {
    const result = {};
    const now = Date.now();
    for (const [sessionId, state] of sessionAttentionStates) {
      if (now - state.lastStatusChangeAt > SESSION_ATTENTION_MAX_AGE_MS) continue;
      result[sessionId] = {
        needsAttention: state.needsAttention,
        lastUserMessageAt: state.lastUserMessageAt,
        lastStatusChangeAt: state.lastStatusChangeAt,
        status: state.status,
        isViewed: state.viewedByClients.size > 0,
      };
    }
    return result;
  };

  // 查询单个会话的关注状态（快照同款形态），无记录返回 null。
  const getSessionAttentionState = (sessionId) => {
    if (!sessionId) return null;
    const state = sessionAttentionStates.get(sessionId);
    if (!state) return null;
    return {
      needsAttention: state.needsAttention,
      lastUserMessageAt: state.lastUserMessageAt,
      lastStatusChangeAt: state.lastStatusChangeAt,
      status: state.status,
      isViewed: state.viewedByClients.size > 0,
    };
  };

  // 全量活动相位快照：{ sessionId: { type: phase } }。
  const getSessionActivitySnapshot = () => {
    const result = {};
    for (const [sessionId, data] of sessionActivityPhases) {
      result[sessionId] = { type: data.phase };
    }
    return result;
  };

  // 当前处于 busy 相位的会话数量。
  const getActiveSessionCount = () => activeSessionCount;

  // 把所有活动相位强制复位为 idle：清空全部 cooldown 定时器并把
  // activeSessionCount 归零（重启收尾用，不逐会话广播事件）。
  const resetAllSessionActivityToIdle = () => {
    for (const timer of sessionActivityCooldowns.values()) {
      clearTimeout(timer);
    }
    sessionActivityCooldowns.clear();
    activeSessionCount = 0;
    const now = Date.now();
    for (const [sessionId] of sessionActivityPhases) {
      sessionActivityPhases.set(sessionId, { phase: 'idle', updatedAt: now });
    }
  };

  // OpenCode 重启后的中断收尾：收集状态为 busy/retry 或相位为 busy
  // 的会话，逐个落一条“被重启中断”的 idle 状态（reason 为
  // opencode-restart，绕过 5 秒去重）并广播 session.error
  // （MessageAbortedError）；最后复位全部活动相位。返回被中断的
  // sessionId 列表。
  const interruptBusySessionsAfterRestart = () => {
    const interruptedSessionIds = new Set();
    for (const [sessionId, state] of sessionStates) {
      if (state.status === 'busy' || state.status === 'retry') {
        interruptedSessionIds.add(sessionId);
      }
    }
    for (const [sessionId, activity] of sessionActivityPhases) {
      if (activity.phase === 'busy') {
        interruptedSessionIds.add(sessionId);
      }
    }

    const eventId = `opencode-restart-${Date.now()}`;
    for (const sessionId of interruptedSessionIds) {
      updateSessionState(sessionId, 'idle', eventId, {
        message: 'Interrupted by OpenCode restart',
        reason: 'opencode-restart',
      });
      broadcastEvent?.({
        type: 'session.error',
        properties: {
          sessionID: sessionId,
          error: {
            name: 'MessageAbortedError',
            message: 'The running turn was interrupted when OpenCode restarted.',
          },
        },
      });
    }

    resetAllSessionActivityToIdle();
    return { sessionIds: [...interruptedSessionIds] };
  };

  // 定时清理：删除超过各自最长保留时长（24 小时）的会话状态、关注
  // 状态与活动相位条目；清理 busy 相位条目时同步扣减 activeSessionCount。
  const cleanupOldSessionStates = () => {
    const now = Date.now();
    for (const [sessionId, data] of sessionStates) {
      if (now - data.lastUpdateAt > SESSION_STATE_MAX_AGE_MS) {
        sessionStates.delete(sessionId);
      }
    }
    for (const [sessionId, state] of sessionAttentionStates) {
      if (now - state.lastStatusChangeAt > SESSION_ATTENTION_MAX_AGE_MS) {
        sessionAttentionStates.delete(sessionId);
      }
    }
    for (const [sessionId, data] of sessionActivityPhases) {
      if (now - data.updatedAt <= SESSION_ACTIVITY_MAX_AGE_MS) continue;
      const timer = sessionActivityCooldowns.get(sessionId);
      if (timer) clearTimeout(timer);
      sessionActivityCooldowns.delete(sessionId);
      sessionActivityPhases.delete(sessionId);
      if (data.phase === 'busy') activeSessionCount = Math.max(0, activeSessionCount - 1);
    }
  };

  // 每小时执行一次过期清理的定时器，dispose 时清除。
  const cleanupInterval = setInterval(cleanupOldSessionStates, SESSION_STATE_CLEANUP_INTERVAL_MS);

  // OpenCode SSE 事件入口：识别 session.status 后联动活动相位
  // （busy/retry -> busy，idle -> cooldown），再更新会话状态与关注
  // 标记；非状态事件直接忽略。
  const processOpenCodeSsePayload = (payload) => {
    const update = extractSessionStatusUpdate(payload);
    if (!update) return;

    if (update.type === 'busy' || update.type === 'retry') {
      setSessionActivityPhase(update.sessionId, 'busy');
    } else if (update.type === 'idle') {
      setSessionActivityPhase(update.sessionId, 'cooldown');
    }

    updateSessionState(update.sessionId, update.type, update.eventId || `sse-${Date.now()}`, {
      attempt: update.attempt,
      message: update.message,
      next: update.next,
    });
  };

  // 释放运行时：停掉清理定时器与全部 cooldown 定时器，清空各状态表
  // 并复位计数（测试与进程关停时调用）。
  const dispose = () => {
    clearInterval(cleanupInterval);
    for (const timer of sessionActivityCooldowns.values()) {
      clearTimeout(timer);
    }
    sessionActivityCooldowns.clear();
    sessionActivityPhases.clear();
    sessionStates.clear();
    sessionAttentionStates.clear();
    activeSessionCount = 0;
  };

  // 导出会话运行时 API（各项职责见上方对应注释）。
  return {
    processOpenCodeSsePayload,
    getSessionActivitySnapshot,
    getActiveSessionCount,
    getSessionStateSnapshot,
    getSessionAttentionSnapshot,
    getSessionState,
    getSessionAttentionState,
    markSessionViewed,
    markSessionUnviewed,
    markUserMessageSent,
    resetAllSessionActivityToIdle,
    interruptBusySessionsAfterRestart,
    dispose,
  };
};
